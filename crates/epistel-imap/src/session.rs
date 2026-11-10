//! One client's conversation: what state it is in and what each command does.
//!
//! This is the read path. Every mailbox opens read-only, so nothing a client does here changes
//! mail; the commands that change it are refused until the write path exists.

use std::io;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};

use epistel_core::model::{AccountState, Mailbox, MailboxName, MailboxRole, MessageRecord, SystemFlags, Uid, UidRange};
use epistel_core::{AccountId, BlobStore, CredentialId, EmailAddress, Error, MetadataStore, password};
use imap_codec::imap_types::auth::AuthMechanism;
use imap_codec::imap_types::command::{Command, CommandBody};
use imap_codec::imap_types::core::Charset;
use imap_codec::imap_types::fetch::{Macro, MacroOrMessageDataItemNames, MessageDataItemName};
use imap_codec::imap_types::mailbox::{ListMailbox, Mailbox as WireMailbox};
use imap_codec::imap_types::search::SearchKey;
use imap_codec::imap_types::sequence::SequenceSet;
use imap_codec::imap_types::status::StatusDataItemName;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tracing::{info, warn};

use crate::wire::Out;
use crate::{fetch, mime, search};

/// What the server announces. Only what is implemented.
pub const CAPABILITIES: &str = "IMAP4rev1 AUTH=PLAIN SASL-IR LITERAL+ ID ENABLE NAMESPACE UNSELECT SPECIAL-USE";

/// Responses are sent to the client once this much has piled up, so that fetching a large
/// mailbox does not hold all of it in memory.
const FLUSH_AT: usize = 64 * 1024;

/// What a connection needs from the rest of the server.
pub struct Context {
    pub metadata: Arc<dyn MetadataStore>,
    pub blobs: Arc<dyn BlobStore>,
    /// How long a failed login is held back before it is answered.
    pub failed_login_delay: Duration,
}

/// What the connection does after a command.
pub(crate) enum Next {
    Continue,
    /// The client still has to send its credentials for `AUTHENTICATE PLAIN`.
    AuthData {
        tag: String,
    },
    Close,
}

/// A command that could not be completed.
enum Failure {
    /// The connection is gone.
    Io(io::Error),
    /// A backend failed. The client is told, the connection stays.
    Backend(Error),
}

impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        Self::Backend(error)
    }
}

type Outcome<T = ()> = Result<T, Failure>;

/// The open mailbox. Sequence number `n` is `uids[n - 1]`.
struct Selected {
    mailbox: Mailbox,
    uids: Vec<Uid>,
}

pub(crate) struct Session<W> {
    context: Arc<Context>,
    writer: W,
    out: Out,
    account: Option<AccountId>,
    selected: Option<Selected>,
}

/// A hash to spend time on when there is no account to check, so that the answer takes as long
/// for an unknown user as for a wrong password.
fn decoy_hash() -> &'static str {
    static HASH: OnceLock<String> = OnceLock::new();
    HASH.get_or_init(|| password::hash(&password::generate()))
}

fn wire_name(mailbox: &WireMailbox<'_>) -> Option<MailboxName> {
    match mailbox {
        WireMailbox::Inbox => Some(MailboxName::inbox()),
        WireMailbox::Other(other) => MailboxName::parse(std::str::from_utf8(other.inner().as_ref()).ok()?).ok(),
    }
}

/// Matches a LIST pattern: `*` is anything, `%` is anything within one level.
fn wildcard(pattern: &[u8], name: &[u8]) -> bool {
    match pattern.split_first() {
        None => name.is_empty(),
        Some((b'*', rest)) => (0..=name.len()).any(|skip| wildcard(rest, &name[skip..])),
        Some((b'%', rest)) => {
            let level = name.iter().position(|byte| *byte == b'/').unwrap_or(name.len());
            (0..=level).any(|skip| wildcard(rest, &name[skip..]))
        }
        Some((byte, rest)) => name.first() == Some(byte) && wildcard(rest, &name[1..]),
    }
}

fn special_use(role: Option<MailboxRole>) -> Option<&'static str> {
    match role? {
        MailboxRole::Drafts => Some("\\Drafts"),
        MailboxRole::Sent => Some("\\Sent"),
        MailboxRole::Junk => Some("\\Junk"),
        MailboxRole::Trash => Some("\\Trash"),
        MailboxRole::Archive => Some("\\Archive"),
        MailboxRole::Inbox | MailboxRole::Quarantine => None,
    }
}

/// The messages of a snapshot that a set names, with their sequence numbers.
fn resolve(uids: &[Uid], set: &SequenceSet, by_uid: bool) -> Vec<(u32, Uid)> {
    let last_seq = uids.len() as u32;
    let last_uid = uids.last().map_or(0, |uid| uid.get());
    let listed = uids.iter().enumerate().filter(|(index, uid)| {
        if by_uid { search::in_set(set, uid.get(), last_uid) } else { search::in_set(set, *index as u32 + 1, last_seq) }
    });
    listed.map(|(index, uid)| (index as u32 + 1, *uid)).collect()
}

impl<W: AsyncWrite + Unpin> Session<W> {
    pub(crate) fn new(context: Arc<Context>, writer: W) -> Self {
        Self { context, writer, out: Out::default(), account: None, selected: None }
    }

    pub(crate) fn authenticated(&self) -> bool {
        self.account.is_some()
    }

    async fn flush(&mut self) -> io::Result<()> {
        self.writer.write_all(&self.out.bytes).await?;
        self.out.bytes.clear();
        self.writer.flush().await
    }

    fn untagged(&mut self, text: &str) {
        self.out.raw("* ").raw(text).crlf();
    }

    fn tagged(&mut self, tag: &str, text: &str) {
        self.out.raw(tag).sp().raw(text).crlf();
    }

    /// Sends one line outside any command.
    pub(crate) async fn say(&mut self, text: &str) -> io::Result<()> {
        self.out.raw(text).crlf();
        self.flush().await
    }

    pub(crate) async fn greet(&mut self) -> io::Result<()> {
        self.say(&format!("* OK [CAPABILITY {CAPABILITIES}] Epistel ready")).await
    }

    /// Runs one command and sends everything it produced.
    pub(crate) async fn handle(&mut self, command: Command<'_>) -> io::Result<Next> {
        let tag = command.tag.inner().to_owned();
        let next = match self.dispatch(&tag, command.body).await {
            Ok(next) => next,
            Err(Failure::Io(error)) => return Err(error),
            Err(Failure::Backend(error)) => {
                warn!(%error, "a command failed on a backend");
                self.tagged(&tag, "NO [SERVERBUG] The server could not complete the command");
                Next::Continue
            }
        };
        self.flush().await?;
        Ok(next)
    }

    async fn dispatch(&mut self, tag: &str, body: CommandBody<'_>) -> Outcome<Next> {
        // In any state.
        match &body {
            CommandBody::Capability => {
                self.untagged(&format!("CAPABILITY {CAPABILITIES}"));
                self.tagged(tag, "OK Capability completed");
                return Ok(Next::Continue);
            }
            CommandBody::Noop | CommandBody::Check => {
                self.refresh().await?;
                self.tagged(tag, "OK Completed");
                return Ok(Next::Continue);
            }
            CommandBody::Logout => {
                self.untagged("BYE Logging out");
                self.tagged(tag, "OK Logout completed");
                return Ok(Next::Close);
            }
            CommandBody::Id { .. } => {
                self.untagged(concat!("ID (\"name\" \"Epistel\" \"version\" \"", env!("CARGO_PKG_VERSION"), "\")"));
                self.tagged(tag, "OK ID completed");
                return Ok(Next::Continue);
            }
            _ => {}
        }

        let Some(account) = self.account else {
            return match body {
                CommandBody::Login { username, password } => {
                    self.log_in(tag, username.as_ref(), password.declassify().as_ref()).await?;
                    Ok(Next::Continue)
                }
                CommandBody::Authenticate { mechanism: AuthMechanism::Plain, initial_response } => {
                    match initial_response {
                        Some(response) => {
                            self.authenticate_plain(tag, response.declassify()).await?;
                            Ok(Next::Continue)
                        }
                        None => {
                            self.out.raw("+ ").crlf();
                            Ok(Next::AuthData { tag: tag.to_owned() })
                        }
                    }
                }
                CommandBody::Authenticate { .. } => {
                    self.tagged(tag, "NO [CANNOT] Only the PLAIN mechanism is offered");
                    Ok(Next::Continue)
                }
                _ => {
                    self.tagged(tag, "BAD Log in first");
                    Ok(Next::Continue)
                }
            };
        };

        match body {
            CommandBody::Login { .. } | CommandBody::Authenticate { .. } => self.tagged(tag, "BAD Already logged in"),
            CommandBody::Enable { .. } => {
                // No extension that has to be enabled is implemented, so nothing is.
                self.untagged("ENABLED");
                self.tagged(tag, "OK Enable completed");
            }
            CommandBody::Namespace => {
                self.untagged("NAMESPACE ((\"\" \"/\")) NIL NIL");
                self.tagged(tag, "OK Namespace completed");
            }
            CommandBody::List { reference, mailbox_wildcard } => {
                self.list(account, &reference, &mailbox_wildcard, false).await?;
                self.tagged(tag, "OK List completed");
            }
            CommandBody::Lsub { reference, mailbox_wildcard } => {
                self.list(account, &reference, &mailbox_wildcard, true).await?;
                self.tagged(tag, "OK Lsub completed");
            }
            CommandBody::Status { mailbox, item_names } => self.status(tag, account, &mailbox, &item_names).await?,
            CommandBody::Select { mailbox, .. } | CommandBody::Examine { mailbox, .. } => {
                self.select(tag, account, &mailbox).await?;
            }
            CommandBody::Unselect | CommandBody::Close => match self.selected.take() {
                // Nothing to expunge on the way out: the mailbox was open read-only.
                Some(_) => self.tagged(tag, "OK Closed"),
                None => self.tagged(tag, "BAD No mailbox is selected"),
            },
            CommandBody::Fetch { sequence_set, macro_or_item_names, uid, .. } => {
                let mut items = match macro_or_item_names {
                    MacroOrMessageDataItemNames::MessageDataItemNames(items) => items,
                    MacroOrMessageDataItemNames::Macro(name) => {
                        use MessageDataItemName::{Body, Envelope, Flags, InternalDate, Rfc822Size};
                        let mut items = vec![Flags, InternalDate, Rfc822Size];
                        if matches!(name, Macro::All | Macro::Full) {
                            items.push(Envelope);
                        }
                        if matches!(name, Macro::Full) {
                            items.push(Body);
                        }
                        items
                    }
                };
                // A fetch by UID always reports the UID.
                if uid && !items.iter().any(|item| matches!(item, MessageDataItemName::Uid)) {
                    items.insert(0, MessageDataItemName::Uid);
                }
                self.fetch(tag, &sequence_set, &items, uid).await?;
            }
            CommandBody::Search { charset, criteria, uid } => {
                self.search(tag, charset.as_ref(), criteria.as_ref(), uid).await?;
            }
            CommandBody::Create { .. }
            | CommandBody::Delete { .. }
            | CommandBody::Rename { .. }
            | CommandBody::Subscribe { .. }
            | CommandBody::Unsubscribe { .. }
            | CommandBody::Append { .. }
            | CommandBody::Expunge
            | CommandBody::ExpungeUid { .. }
            | CommandBody::Store { .. }
            | CommandBody::Copy { .. }
            | CommandBody::Move { .. } => {
                self.tagged(tag, "NO [CANNOT] This server is read-only for now");
            }
            _ => self.tagged(tag, "BAD Command is not supported"),
        }
        Ok(Next::Continue)
    }

    // -- logging in ------------------------------------------------------------------------------

    /// Finishes `AUTHENTICATE PLAIN` with the decoded credentials: authorization identity, user
    /// name and password, separated by zero bytes.
    pub(crate) async fn authenticate_plain(&mut self, tag: &str, response: &[u8]) -> io::Result<()> {
        let mut fields = response.split(|byte| *byte == 0);
        let (Some(authorize_as), Some(login), Some(secret), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            self.tagged(tag, "BAD The PLAIN response is malformed");
            return self.flush().await;
        };
        if !authorize_as.is_empty() && authorize_as != login {
            self.tagged(tag, "NO [AUTHORIZATIONFAILED] Acting for another user is not offered");
            return self.flush().await;
        }
        match self.log_in(tag, login, secret).await {
            Ok(()) => self.flush().await,
            Err(Failure::Io(error)) => Err(error),
            Err(Failure::Backend(error)) => {
                warn!(%error, "a login failed on a backend");
                self.tagged(tag, "NO [UNAVAILABLE] Try again later");
                self.flush().await
            }
        }
    }

    async fn log_in(&mut self, tag: &str, login: &[u8], secret: &[u8]) -> Outcome {
        match self.check(login, secret).await? {
            Some(account) => {
                self.account = Some(account);
                info!(%account, "logged in");
                self.tagged(tag, &format!("OK [CAPABILITY {CAPABILITIES}] Logged in"));
            }
            None => {
                // Hold the answer back: it slows guessing down without any bookkeeping.
                tokio::time::sleep(self.context.failed_login_delay).await;
                self.tagged(tag, "NO [AUTHENTICATIONFAILED] Invalid credentials");
            }
        }
        Ok(())
    }

    /// The account the credentials belong to, if they are right.
    async fn check(&self, login: &[u8], secret: &[u8]) -> Result<Option<AccountId>, Error> {
        let store = &*self.context.metadata;
        let login = std::str::from_utf8(login).ok().and_then(|text| EmailAddress::parse(text).ok());
        let account = match &login {
            Some(login) => store.find_account_by_login(login).await?,
            None => None,
        };
        let account = account.filter(|account| account.state == AccountState::Active);
        let mut hashes: Vec<(Option<CredentialId>, String)> = match &account {
            Some(account) => {
                let credentials = store.list_credentials(account.id).await?;
                credentials.into_iter().map(|credential| (Some(credential.id), credential.secret_hash)).collect()
            }
            None => Vec::new(),
        };
        if hashes.is_empty() {
            hashes.push((None, decoy_hash().to_owned()));
        }
        let secret = String::from_utf8_lossy(secret).into_owned();
        // Verifying is slow by design; keep it off the threads that serve the other connections.
        let verifying = tokio::task::spawn_blocking(move || {
            hashes.into_iter().find(|(_, hash)| password::verify(&secret, hash)).and_then(|(id, _)| id)
        });
        let matched = verifying.await.map_err(Error::unavailable)?;
        let (Some(account), Some(credential)) = (account, matched) else { return Ok(None) };
        if let Err(error) = store.touch_credential(account.id, credential, SystemTime::now()).await {
            warn!(%error, "could not record the use of a credential");
        }
        Ok(Some(account.id))
    }

    // -- mailboxes -------------------------------------------------------------------------------

    /// The account's mailbox of that name, unless it is one clients are not shown.
    async fn visible(&self, account: AccountId, mailbox: &WireMailbox<'_>) -> Result<Option<Mailbox>, Error> {
        let Some(name) = wire_name(mailbox) else { return Ok(None) };
        let found = self.context.metadata.find_mailbox(account, &name).await?;
        Ok(found.filter(|mailbox| mailbox.role != Some(MailboxRole::Quarantine)))
    }

    async fn list(
        &mut self,
        account: AccountId,
        reference: &WireMailbox<'_>,
        pattern: &ListMailbox<'_>,
        subscribed_only: bool,
    ) -> Outcome {
        let command = if subscribed_only { "LSUB" } else { "LIST" };
        let pattern: &[u8] = match pattern {
            ListMailbox::Token(token) => token.as_ref(),
            ListMailbox::String(text) => text.as_ref(),
        };
        if pattern.is_empty() {
            // The way to ask for the hierarchy separator.
            self.untagged(&format!("{command} (\\Noselect) \"/\" \"\""));
            return Ok(());
        }
        let mut full: Vec<u8> = match reference {
            WireMailbox::Inbox => b"INBOX".to_vec(),
            WireMailbox::Other(other) => other.inner().as_ref().to_vec(),
        };
        full.extend_from_slice(pattern);
        // INBOX is the one name that matches in any case.
        let first_level = full.iter().position(|byte| *byte == b'/').unwrap_or(full.len());
        if full[..first_level].eq_ignore_ascii_case(b"INBOX") {
            full[..first_level].copy_from_slice(b"INBOX");
        }

        let mailboxes = self.context.metadata.list_mailboxes(account).await?;
        let shown: Vec<&Mailbox> = mailboxes
            .iter()
            .filter(|mailbox| mailbox.role != Some(MailboxRole::Quarantine))
            .filter(|mailbox| !subscribed_only || mailbox.subscribed)
            .collect();
        for mailbox in &shown {
            let name = mailbox.name.as_str();
            if !wildcard(&full, name.as_bytes()) {
                continue;
            }
            let below = format!("{name}/");
            let has_children = shown.iter().any(|other| other.name.as_str().starts_with(&below));
            self.out.raw("* ").raw(command).raw(" (");
            if !subscribed_only {
                self.out.raw(if has_children { "\\HasChildren" } else { "\\HasNoChildren" });
                if let Some(attribute) = special_use(mailbox.role) {
                    self.out.sp().raw(attribute);
                }
            }
            self.out.raw(") \"/\" ").string(name.as_bytes()).crlf();
        }
        Ok(())
    }

    async fn status(
        &mut self,
        tag: &str,
        account: AccountId,
        mailbox: &WireMailbox<'_>,
        items: &[StatusDataItemName],
    ) -> Outcome {
        let Some(mailbox) = self.visible(account, mailbox).await? else {
            self.tagged(tag, "NO [NONEXISTENT] No such mailbox");
            return Ok(());
        };
        let records = self.context.metadata.messages(mailbox.id, UidRange::ALL).await?;
        // Read the counters after the records, so that UIDNEXT is never behind what was counted.
        let mailbox = self.context.metadata.get_mailbox(mailbox.id).await?;
        let count = |flag: SystemFlags, set: bool| {
            records.iter().filter(|record| record.flags.system.contains(flag) == set).count() as u64
        };
        self.out.raw("* STATUS ").string(mailbox.name.as_str().as_bytes()).raw(" (");
        let mut first = true;
        for item in items {
            let (name, value) = match item {
                StatusDataItemName::Messages => ("MESSAGES", records.len() as u64),
                StatusDataItemName::Recent => ("RECENT", 0),
                StatusDataItemName::UidNext => ("UIDNEXT", u64::from(mailbox.next_uid.get())),
                StatusDataItemName::UidValidity => ("UIDVALIDITY", u64::from(mailbox.uid_validity.0.get())),
                StatusDataItemName::Unseen => ("UNSEEN", count(SystemFlags::SEEN, false)),
                StatusDataItemName::Deleted => ("DELETED", count(SystemFlags::DELETED, true)),
                _ => continue,
            };
            if !std::mem::take(&mut first) {
                self.out.sp();
            }
            self.out.raw(name).sp().number(value);
        }
        self.out.raw(")").crlf();
        self.tagged(tag, "OK Status completed");
        Ok(())
    }

    async fn select(&mut self, tag: &str, account: AccountId, mailbox: &WireMailbox<'_>) -> Outcome {
        // Selecting closes whatever was open, also when the new mailbox does not exist.
        self.selected = None;
        let Some(mailbox) = self.visible(account, mailbox).await? else {
            self.tagged(tag, "NO [NONEXISTENT] No such mailbox");
            return Ok(());
        };
        let records = self.context.metadata.messages(mailbox.id, UidRange::ALL).await?;
        let mailbox = self.context.metadata.get_mailbox(mailbox.id).await?;
        self.untagged(&format!("{} EXISTS", records.len()));
        self.untagged("0 RECENT");
        self.untagged("FLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft)");
        self.untagged("OK [PERMANENTFLAGS ()] No flags can be changed");
        let unseen = records.iter().position(|record| !record.flags.system.contains(SystemFlags::SEEN));
        if let Some(index) = unseen {
            self.untagged(&format!("OK [UNSEEN {}] First unseen message", index + 1));
        }
        self.untagged(&format!("OK [UIDVALIDITY {}] UIDs valid", mailbox.uid_validity.0));
        self.untagged(&format!("OK [UIDNEXT {}] Predicted next UID", mailbox.next_uid));
        self.tagged(tag, "OK [READ-ONLY] Selected");
        self.selected = Some(Selected { mailbox, uids: records.iter().map(|record| record.uid).collect() });
        Ok(())
    }

    /// Tells the client about messages that arrived since the mailbox was opened or last checked.
    async fn refresh(&mut self) -> Outcome {
        let Some(selected) = &mut self.selected else { return Ok(()) };
        let after = selected.uids.last().and_then(|uid| uid.next()).unwrap_or(Uid::FIRST);
        let range = UidRange { first: after, last: Uid::MAX };
        let arrived = self.context.metadata.messages(selected.mailbox.id, range).await?;
        if !arrived.is_empty() {
            selected.uids.extend(arrived.iter().map(|record| record.uid));
            let total = selected.uids.len();
            self.untagged(&format!("{total} EXISTS"));
        }
        Ok(())
    }

    // -- messages --------------------------------------------------------------------------------

    /// The records of the named messages of the open mailbox, with their sequence numbers.
    /// `None` when no mailbox is open.
    async fn records(&self, set: Option<(&SequenceSet, bool)>) -> Result<Option<Vec<(u32, MessageRecord)>>, Error> {
        let Some(selected) = &self.selected else { return Ok(None) };
        let targets: Vec<(u32, Uid)> = match set {
            Some((set, by_uid)) => resolve(&selected.uids, set, by_uid),
            None => selected.uids.iter().enumerate().map(|(index, uid)| (index as u32 + 1, *uid)).collect(),
        };
        let (Some(first), Some(last)) = (targets.first(), targets.last()) else { return Ok(Some(Vec::new())) };
        let range = UidRange { first: first.1, last: last.1 };
        let mut stored = self.context.metadata.messages(selected.mailbox.id, range).await?.into_iter().peekable();
        // Both lists ascend by UID. A message removed by someone else is simply not there.
        let mut found = Vec::with_capacity(targets.len());
        for (seq, uid) in targets {
            while stored.next_if(|record| record.uid < uid).is_some() {}
            if let Some(record) = stored.next_if(|record| record.uid == uid) {
                found.push((seq, record));
            }
        }
        Ok(Some(found))
    }

    /// The raw message of a record. `None` when the blob is missing, which is logged.
    async fn raw(&self, record: &MessageRecord) -> Result<Option<bytes::Bytes>, Error> {
        match self.context.blobs.get(record.blob).await {
            Ok(raw) => Ok(Some(raw)),
            Err(Error::NotFound) => {
                warn!(mailbox = %record.mailbox, uid = %record.uid, "a message record points at a missing blob");
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    async fn fetch(
        &mut self,
        tag: &str,
        set: &SequenceSet,
        items: &[MessageDataItemName<'_>],
        by_uid: bool,
    ) -> Outcome {
        if !items.iter().all(fetch::supported) {
            self.tagged(tag, "NO [CANNOT] BINARY content is not offered");
            return Ok(());
        }
        let Some(records) = self.records(Some((set, by_uid))).await? else {
            self.tagged(tag, "BAD No mailbox is selected");
            return Ok(());
        };
        let wants_message = items.iter().any(fetch::needs_message);
        for (seq, record) in &records {
            let raw = if wants_message { self.raw(record).await? } else { None };
            if wants_message && raw.is_none() {
                continue;
            }
            let root = raw.as_deref().map(mime::parse);
            let message = raw.as_deref().zip(root.as_ref());
            self.out.raw(&format!("* {seq} FETCH ("));
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    self.out.sp();
                }
                fetch::write_item(&mut self.out, item, record, message);
            }
            self.out.raw(")").crlf();
            if self.out.bytes.len() >= FLUSH_AT {
                self.flush().await?;
            }
        }
        self.tagged(tag, "OK Fetch completed");
        Ok(())
    }

    async fn search(
        &mut self,
        tag: &str,
        charset: Option<&Charset<'_>>,
        criteria: &[SearchKey<'_>],
        by_uid: bool,
    ) -> Outcome {
        let known = match charset {
            None => true,
            Some(Charset::Atom(name)) => {
                ["US-ASCII", "UTF-8"].iter().any(|known| name.inner().eq_ignore_ascii_case(known))
            }
            Some(Charset::Quoted(name)) => {
                ["US-ASCII", "UTF-8"].iter().any(|known| name.inner().eq_ignore_ascii_case(known))
            }
        };
        if !known {
            self.tagged(tag, "NO [BADCHARSET (US-ASCII UTF-8)] That character set is not offered");
            return Ok(());
        }
        let Some(records) = self.records(None).await? else {
            self.tagged(tag, "BAD No mailbox is selected");
            return Ok(());
        };
        let last_seq = records.last().map_or(0, |(seq, _)| *seq);
        let last_uid = records.last().map_or(0, |(_, record)| record.uid.get());
        let wants_message = criteria.iter().any(search::needs_message);
        let mut hits = Vec::new();
        for (seq, record) in &records {
            let raw = if wants_message { self.raw(record).await? } else { None };
            let root = raw.as_deref().map(mime::parse);
            let candidate =
                search::Candidate { seq: *seq, record, message: raw.as_deref().zip(root.as_ref()), last_seq, last_uid };
            if criteria.iter().all(|key| search::matches(key, &candidate)) {
                hits.push(if by_uid { record.uid.get() } else { *seq });
            }
        }
        self.out.raw("* SEARCH");
        for hit in hits {
            self.out.sp().number(u64::from(hit));
        }
        self.out.crlf();
        self.tagged(tag, "OK Search completed");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_patterns() {
        let matches = |pattern: &str, name: &str| wildcard(pattern.as_bytes(), name.as_bytes());
        assert!(matches("*", "INBOX") && matches("*", "Work/Projects/2026"));
        assert!(matches("%", "INBOX") && !matches("%", "Work/Projects"));
        assert!(matches("Work/%", "Work/Projects") && !matches("Work/%", "Work/Projects/2026"));
        assert!(matches("Work/*", "Work/Projects/2026") && !matches("Work/*", "Work"));
        assert!(matches("*2026", "Work/Projects/2026") && matches("S%t", "Sent") && !matches("S%t", "Sen"));
        assert!(matches("INBOX", "INBOX") && !matches("INBOX", "INBOX/x") && !matches("", "INBOX"));
    }
}
