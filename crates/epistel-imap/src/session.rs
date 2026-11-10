//! One client's conversation: what state it is in and what each command does.

use std::io;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use epistel_core::model::{
    AccountState, FlagChange, Flags, Keyword, Mailbox, MailboxName, MailboxRole, MessageRecord, ModSeq, NewMessage,
    SystemFlags, Uid, UidRange,
};
use epistel_core::{
    AccountId, BlobId, BlobStore, ChangeBus, ChangeKind, CredentialId, EmailAddress, Error, MailboxChange,
    MetadataStore, Subscription, password,
};
use imap_codec::imap_types::auth::AuthMechanism;
use imap_codec::imap_types::command::{Command, CommandBody};
use imap_codec::imap_types::core::Charset;
use imap_codec::imap_types::extensions::binary::LiteralOrLiteral8;
use imap_codec::imap_types::fetch::{Macro, MacroOrMessageDataItemNames, MessageDataItemName};
use imap_codec::imap_types::flag::{Flag, StoreResponse, StoreType};
use imap_codec::imap_types::mailbox::{ListMailbox, Mailbox as WireMailbox};
use imap_codec::imap_types::search::SearchKey;
use imap_codec::imap_types::sequence::SequenceSet;
use imap_codec::imap_types::status::StatusDataItemName;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tracing::{info, warn};

use crate::wire::Out;
use crate::{fetch, mime, search};

/// What the server announces. Only what is implemented.
pub const CAPABILITIES: &str =
    "IMAP4rev1 AUTH=PLAIN SASL-IR LITERAL+ ID ENABLE NAMESPACE UNSELECT SPECIAL-USE IDLE UIDPLUS MOVE";

/// Responses are sent to the client once this much has piled up, so that fetching a large
/// mailbox does not hold all of it in memory.
const FLUSH_AT: usize = 64 * 1024;

/// What a connection needs from the rest of the server.
pub struct Context {
    pub metadata: Arc<dyn MetadataStore>,
    pub blobs: Arc<dyn BlobStore>,
    pub changes: Arc<dyn ChangeBus>,
    /// How long a failed login is held back before it is answered.
    pub failed_login_delay: Duration,
    /// The largest message a client may append, in bytes.
    pub max_message_size: u64,
}

/// What the connection does after a command.
pub(crate) enum Next {
    Continue,
    /// The client still has to send its credentials for `AUTHENTICATE PLAIN`.
    AuthData {
        tag: String,
    },
    /// The client waits for news until it says `DONE`.
    Idle {
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

/// The open mailbox as this client was last told about it. Sequence number `n` is
/// `entries[n - 1]`; the modification sequence is the one the client knows the flags of.
struct Selected {
    mailbox: Mailbox,
    entries: Vec<(Uid, ModSeq)>,
    read_only: bool,
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

fn wire_text(mailbox: &WireMailbox<'_>) -> Option<String> {
    match mailbox {
        WireMailbox::Inbox => Some("INBOX".to_owned()),
        WireMailbox::Other(other) => String::from_utf8(other.inner().as_ref().to_vec()).ok(),
    }
}

fn wire_name(mailbox: &WireMailbox<'_>) -> Option<MailboxName> {
    MailboxName::parse(&wire_text(mailbox)?).ok()
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
fn resolve(entries: &[(Uid, ModSeq)], set: &SequenceSet, by_uid: bool) -> Vec<(u32, Uid)> {
    let last_seq = entries.len() as u32;
    let last_uid = entries.last().map_or(0, |(uid, _)| uid.get());
    let listed = entries.iter().enumerate().filter(|(index, (uid, _))| {
        if by_uid { search::in_set(set, uid.get(), last_uid) } else { search::in_set(set, *index as u32 + 1, last_seq) }
    });
    listed.map(|(index, (uid, _))| (index as u32 + 1, *uid)).collect()
}

/// The flags a client named. `None` when one of them is a system flag this server does not know.
fn flags_from(wire: &[Flag<'_>]) -> Option<Flags> {
    let mut flags = Flags::default();
    for flag in wire {
        let system = match flag {
            Flag::Answered => SystemFlags::ANSWERED,
            Flag::Deleted => SystemFlags::DELETED,
            Flag::Draft => SystemFlags::DRAFT,
            Flag::Flagged => SystemFlags::FLAGGED,
            Flag::Seen => SystemFlags::SEEN,
            Flag::Keyword(atom) => {
                flags.keywords.insert(Keyword::parse(atom.as_ref()).ok()?);
                continue;
            }
            Flag::Extension(_) => return None,
        };
        flags.system = flags.system | system;
    }
    Some(flags)
}

/// UIDs as a set on the wire, runs written as ranges: `3:5,9`.
fn uid_set(uids: &[Uid]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut run: Option<(u32, u32)> = None;
    for uid in uids.iter().map(|uid| uid.get()).chain(std::iter::once(0)) {
        match run {
            Some((first, last)) if uid == last + 1 => run = Some((first, uid)),
            _ => {
                if let Some((first, last)) = run {
                    parts.push(if first == last { first.to_string() } else { format!("{first}:{last}") });
                }
                run = (uid != 0).then_some((uid, uid));
            }
        }
    }
    parts.join(",")
}

/// Whether fetching the item counts as reading the message.
fn marks_as_read(item: &MessageDataItemName<'_>) -> bool {
    matches!(
        item,
        MessageDataItemName::BodyExt { peek: false, .. }
            | MessageDataItemName::Rfc822
            | MessageDataItemName::Rfc822Text
    )
}

impl<W: AsyncWrite + Unpin> Session<W> {
    pub(crate) fn new(context: Arc<Context>, writer: W) -> Self {
        Self { context, writer, out: Out::default(), account: None, selected: None }
    }

    /// The longest literal the client may send now. Before login there is no reason for a long one.
    pub(crate) fn longest_literal(&self) -> usize {
        match self.account {
            Some(_) => usize::try_from(self.context.max_message_size).unwrap_or(usize::MAX),
            None => 64 * 1024,
        }
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
                self.sync().await?;
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
            CommandBody::Idle => {
                self.out.raw("+ idling").crlf();
                return Ok(Next::Idle { tag: tag.to_owned() });
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
            CommandBody::Select { mailbox, .. } => self.select(tag, account, &mailbox, false).await?,
            CommandBody::Examine { mailbox, .. } => self.select(tag, account, &mailbox, true).await?,
            CommandBody::Create { mailbox } => self.create(tag, account, &mailbox).await?,
            CommandBody::Delete { mailbox } => self.delete(tag, account, &mailbox).await?,
            CommandBody::Rename { from, to } => self.rename(tag, account, &from, &to).await?,
            CommandBody::Subscribe { mailbox } => self.subscribe(tag, account, &mailbox, true).await?,
            CommandBody::Unsubscribe { mailbox } => self.subscribe(tag, account, &mailbox, false).await?,
            CommandBody::Append { mailbox, flags, date, message } => {
                let data: &[u8] = match &message {
                    LiteralOrLiteral8::Literal(literal) => literal.as_ref(),
                    LiteralOrLiteral8::Literal8(literal) => &literal.data,
                };
                let received = date.map(|date| SystemTime::from(*date.as_ref()));
                self.append(tag, account, &mailbox, &flags, received, data).await?;
            }
            CommandBody::Unselect => match self.selected.take() {
                Some(_) => self.tagged(tag, "OK Unselected"),
                None => self.tagged(tag, "BAD No mailbox is selected"),
            },
            CommandBody::Close => {
                if self.selected.is_none() {
                    self.tagged(tag, "BAD No mailbox is selected");
                } else {
                    // Closing removes the messages marked deleted, without saying which.
                    self.remove_deleted(None).await?;
                    self.selected = None;
                    self.tagged(tag, "OK Closed");
                }
            }
            CommandBody::Expunge => self.expunge(tag, None).await?,
            CommandBody::ExpungeUid { sequence_set } => self.expunge(tag, Some(&sequence_set)).await?,
            CommandBody::Store { sequence_set, kind, response, flags, uid, .. } => {
                self.store(tag, &sequence_set, kind, response, &flags, uid).await?;
            }
            CommandBody::Copy { sequence_set, mailbox, uid } => {
                self.copy(tag, account, &sequence_set, &mailbox, uid, false).await?;
            }
            CommandBody::Move { sequence_set, mailbox, uid } => {
                self.copy(tag, account, &sequence_set, &mailbox, uid, true).await?;
            }
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

    /// The account's mailboxes that clients are shown.
    async fn shown(&self, account: AccountId) -> Result<Vec<Mailbox>, Error> {
        let mut mailboxes = self.context.metadata.list_mailboxes(account).await?;
        mailboxes.retain(|mailbox| mailbox.role != Some(MailboxRole::Quarantine));
        Ok(mailboxes)
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

        let mailboxes = self.shown(account).await?;
        let shown: Vec<&Mailbox> = mailboxes.iter().filter(|mailbox| !subscribed_only || mailbox.subscribed).collect();
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

    async fn select(&mut self, tag: &str, account: AccountId, mailbox: &WireMailbox<'_>, read_only: bool) -> Outcome {
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
        if read_only {
            self.untagged("OK [PERMANENTFLAGS ()] No flags can be changed");
        } else {
            self.untagged("OK [PERMANENTFLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft \\*)] Flags and keywords");
        }
        let unseen = records.iter().position(|record| !record.flags.system.contains(SystemFlags::SEEN));
        if let Some(index) = unseen {
            self.untagged(&format!("OK [UNSEEN {}] First unseen message", index + 1));
        }
        self.untagged(&format!("OK [UIDVALIDITY {}] UIDs valid", mailbox.uid_validity.0));
        self.untagged(&format!("OK [UIDNEXT {}] Predicted next UID", mailbox.next_uid));
        self.tagged(tag, if read_only { "OK [READ-ONLY] Selected" } else { "OK [READ-WRITE] Selected" });
        let entries = records.iter().map(|record| (record.uid, record.modseq)).collect();
        self.selected = Some(Selected { mailbox, entries, read_only });
        Ok(())
    }

    async fn create(&mut self, tag: &str, account: AccountId, mailbox: &WireMailbox<'_>) -> Outcome {
        // A trailing separator only says that the client means to create children later.
        let text = wire_text(mailbox).unwrap_or_default();
        let Ok(name) = MailboxName::parse(text.trim_end_matches(MailboxName::DELIMITER)) else {
            self.tagged(tag, "NO [CANNOT] That is not a usable mailbox name");
            return Ok(());
        };
        let store = &*self.context.metadata;
        // The levels above come into being with it, as clients expect.
        let levels: Vec<usize> = name.as_str().match_indices(MailboxName::DELIMITER).map(|(at, _)| at).collect();
        for at in levels {
            let parent = MailboxName::parse(&name.as_str()[..at]).map_err(|_| Error::Conflict("mailbox name"))?;
            match store.create_mailbox(account, parent, None).await {
                Ok(_) | Err(Error::AlreadyExists) => {}
                Err(error) => return Err(error.into()),
            }
        }
        match store.create_mailbox(account, name, None).await {
            Ok(_) => self.tagged(tag, "OK Created"),
            Err(Error::AlreadyExists) => self.tagged(tag, "NO [ALREADYEXISTS] The mailbox exists"),
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }

    async fn delete(&mut self, tag: &str, account: AccountId, mailbox: &WireMailbox<'_>) -> Outcome {
        let Some(mailbox) = self.visible(account, mailbox).await? else {
            self.tagged(tag, "NO [NONEXISTENT] No such mailbox");
            return Ok(());
        };
        if mailbox.role == Some(MailboxRole::Inbox) {
            self.tagged(tag, "NO [CANNOT] The inbox cannot be deleted");
            return Ok(());
        }
        let below = format!("{}/", mailbox.name);
        if self.shown(account).await?.iter().any(|other| other.name.as_str().starts_with(&below)) {
            self.tagged(tag, "NO [HASCHILDREN] Delete the mailboxes inside it first");
            return Ok(());
        }
        let released = self.context.metadata.delete_mailbox(mailbox.id).await?;
        self.discard(released).await;
        if self.selected.as_ref().is_some_and(|selected| selected.mailbox.id == mailbox.id) {
            self.selected = None;
        }
        self.tagged(tag, "OK Deleted");
        Ok(())
    }

    async fn rename(&mut self, tag: &str, account: AccountId, from: &WireMailbox<'_>, to: &WireMailbox<'_>) -> Outcome {
        let Some(mailbox) = self.visible(account, from).await? else {
            self.tagged(tag, "NO [NONEXISTENT] No such mailbox");
            return Ok(());
        };
        if mailbox.role == Some(MailboxRole::Inbox) {
            self.tagged(tag, "NO [CANNOT] The inbox cannot be renamed");
            return Ok(());
        }
        let Some(new_name) = wire_name(to).filter(|name| name.as_str() != "INBOX") else {
            self.tagged(tag, "NO [CANNOT] That is not a usable mailbox name");
            return Ok(());
        };
        let store = &*self.context.metadata;
        let inside = self.shown(account).await?;
        match store.rename_mailbox(mailbox.id, new_name.clone()).await {
            Ok(_) => {}
            Err(Error::AlreadyExists) => {
                self.tagged(tag, "NO [ALREADYEXISTS] The new name is taken");
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        }
        // What was inside moves along.
        let below = format!("{}/", mailbox.name);
        for child in inside.iter().filter(|other| other.name.as_str().starts_with(&below)) {
            let moved = format!("{new_name}/{}", &child.name.as_str()[below.len()..]);
            let renamed = match MailboxName::parse(&moved) {
                Ok(name) => store.rename_mailbox(child.id, name).await.map(|_| ()),
                Err(_) => Err(Error::Conflict("mailbox name")),
            };
            if let Err(error) = renamed {
                warn!(%error, mailbox = %child.name, "could not move a mailbox along with its parent");
            }
        }
        self.tagged(tag, "OK Renamed");
        Ok(())
    }

    async fn subscribe(
        &mut self,
        tag: &str,
        account: AccountId,
        mailbox: &WireMailbox<'_>,
        subscribed: bool,
    ) -> Outcome {
        match self.visible(account, mailbox).await? {
            Some(mailbox) => {
                self.context.metadata.set_subscribed(mailbox.id, subscribed).await?;
                self.tagged(tag, "OK Completed");
            }
            None => self.tagged(tag, "NO [NONEXISTENT] No such mailbox"),
        }
        Ok(())
    }

    // -- keeping the client's picture current ---------------------------------------------------

    async fn publish(&self, mailbox: &Mailbox, kind: ChangeKind) {
        if let Err(error) = self.context.changes.publish(MailboxChange { mailbox: mailbox.id, kind }).await {
            warn!(%error, "could not announce a change");
        }
    }

    /// Deletes blobs nothing points at any more. A failure leaves garbage, nothing worse.
    async fn discard(&self, blobs: Vec<BlobId>) {
        for blob in blobs {
            if let Err(error) = self.context.blobs.delete(blob).await {
                warn!(%error, "could not remove an unused blob");
            }
        }
    }

    /// A subscription to changes of the open mailbox, for the time the client idles.
    pub(crate) async fn watch(&self) -> Option<Subscription> {
        let selected = self.selected.as_ref()?;
        self.context.changes.subscribe(selected.mailbox.id).await.ok()
    }

    /// Brings the client up to date and sends what it had not heard yet.
    pub(crate) async fn announce(&mut self) -> io::Result<()> {
        match self.sync().await {
            Ok(()) => self.flush().await,
            Err(Failure::Io(error)) => Err(error),
            Err(Failure::Backend(error)) => {
                warn!(%error, "could not look for changes");
                Ok(())
            }
        }
    }

    /// Compares the open mailbox with what the client was told and reports the difference:
    /// messages that are gone, messages that arrived, flags that changed.
    async fn sync(&mut self) -> Outcome {
        let Some(selected) = &mut self.selected else { return Ok(()) };
        let records = self.context.metadata.messages(selected.mailbox.id, UidRange::ALL).await?;
        let mut current = records.iter().peekable();
        let mut entries = Vec::with_capacity(records.len());
        let mut changed = Vec::new();
        for (uid, known) in &selected.entries {
            while current.next_if(|record| record.uid < *uid).is_some() {}
            match current.next_if(|record| record.uid == *uid) {
                Some(record) => {
                    entries.push((record.uid, record.modseq));
                    if record.modseq != *known {
                        changed.push((entries.len(), record));
                    }
                }
                // Numbers shift down with every removal, so each is reported at its place by then.
                None => {
                    self.out.raw(&format!("* {} EXPUNGE", entries.len() + 1)).crlf();
                }
            }
        }
        let arrived = current.len();
        entries.extend(current.map(|record| (record.uid, record.modseq)));
        if arrived > 0 {
            self.out.raw(&format!("* {} EXISTS", entries.len())).crlf();
        }
        for (seq, record) in changed {
            self.out.raw(&format!("* {seq} FETCH (UID {} FLAGS ", record.uid));
            fetch::write_flags(&mut self.out, &record.flags);
            self.out.raw(")").crlf();
        }
        selected.entries = entries;
        Ok(())
    }

    /// Records that the client has been told the current flags of a message.
    fn told(&mut self, uid: Uid, modseq: ModSeq) {
        if let Some(selected) = &mut self.selected
            && let Ok(index) = selected.entries.binary_search_by_key(&uid, |(uid, _)| *uid)
        {
            selected.entries[index].1 = modseq;
        }
    }

    // -- messages --------------------------------------------------------------------------------

    /// The records of the named messages of the open mailbox, with their sequence numbers.
    /// `None` when no mailbox is open.
    async fn records(&self, set: Option<(&SequenceSet, bool)>) -> Result<Option<Vec<(u32, MessageRecord)>>, Error> {
        let Some(selected) = &self.selected else { return Ok(None) };
        let targets: Vec<(u32, Uid)> = match set {
            Some((set, by_uid)) => resolve(&selected.entries, set, by_uid),
            None => selected.entries.iter().enumerate().map(|(index, (uid, _))| (index as u32 + 1, *uid)).collect(),
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
    async fn raw(&self, record: &MessageRecord) -> Result<Option<Bytes>, Error> {
        match self.context.blobs.get(record.blob).await {
            Ok(raw) => Ok(Some(raw)),
            Err(Error::NotFound) => {
                warn!(mailbox = %record.mailbox, uid = %record.uid, "a message record points at a missing blob");
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    /// The open mailbox if it may be changed. Answers the command otherwise.
    fn writable(&mut self, tag: &str) -> Option<Mailbox> {
        match &self.selected {
            None => self.tagged(tag, "BAD No mailbox is selected"),
            Some(selected) if selected.read_only => self.tagged(tag, "NO [READ-ONLY] The mailbox is open read-only"),
            Some(selected) => return Some(selected.mailbox.clone()),
        }
        None
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
        let mailbox =
            self.selected.as_ref().filter(|selected| !selected.read_only).map(|selected| selected.mailbox.clone());
        let marks = mailbox.filter(|_| items.iter().any(marks_as_read));
        let reports_flags = items.iter().any(|item| matches!(item, MessageDataItemName::Flags));
        let wants_message = items.iter().any(fetch::needs_message);
        for (seq, record) in records {
            let raw = if wants_message { self.raw(&record).await? } else { None };
            if wants_message && raw.is_none() {
                continue;
            }
            // Reading a message in a mailbox open for writing marks it as read.
            let mut record = record;
            let mut newly_read = false;
            if let Some(mailbox) = &marks
                && !record.flags.system.contains(SystemFlags::SEEN)
            {
                let seen = FlagChange::Add(SystemFlags::SEEN.into());
                match self.context.metadata.update_flags(mailbox.id, record.uid, seen).await {
                    Ok(updated) => {
                        record = updated;
                        newly_read = true;
                        self.publish(mailbox, ChangeKind::FlagsChanged(record.uid)).await;
                    }
                    Err(Error::NotFound) => continue,
                    Err(error) => return Err(error.into()),
                }
            }
            let root = raw.as_deref().map(mime::parse);
            let message = raw.as_deref().zip(root.as_ref());
            self.out.raw(&format!("* {seq} FETCH ("));
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    self.out.sp();
                }
                fetch::write_item(&mut self.out, item, &record, message);
            }
            if newly_read && !reports_flags {
                self.out.raw(" FLAGS ");
                fetch::write_flags(&mut self.out, &record.flags);
            }
            self.out.raw(")").crlf();
            if newly_read || reports_flags {
                self.told(record.uid, record.modseq);
            }
            if self.out.bytes.len() >= FLUSH_AT {
                self.flush().await?;
            }
        }
        self.tagged(tag, "OK Fetch completed");
        Ok(())
    }

    async fn store(
        &mut self,
        tag: &str,
        set: &SequenceSet,
        kind: StoreType,
        response: StoreResponse,
        flags: &[Flag<'_>],
        by_uid: bool,
    ) -> Outcome {
        let Some(mailbox) = self.writable(tag) else { return Ok(()) };
        let Some(flags) = flags_from(flags) else {
            self.tagged(tag, "BAD That flag cannot be stored");
            return Ok(());
        };
        let change = match kind {
            StoreType::Replace => FlagChange::Replace(flags),
            StoreType::Add => FlagChange::Add(flags),
            StoreType::Remove => FlagChange::Remove(flags),
        };
        let targets =
            self.selected.as_ref().map(|selected| resolve(&selected.entries, set, by_uid)).unwrap_or_default();
        for (seq, uid) in targets {
            let record = match self.context.metadata.update_flags(mailbox.id, uid, change.clone()).await {
                Ok(record) => record,
                // Removed by someone else; the client hears of that on its next check.
                Err(Error::NotFound) => continue,
                Err(error) => return Err(error.into()),
            };
            self.publish(&mailbox, ChangeKind::FlagsChanged(uid)).await;
            self.told(uid, record.modseq);
            if matches!(response, StoreResponse::Answer) {
                self.out.raw(&format!("* {seq} FETCH ("));
                if by_uid {
                    self.out.raw(&format!("UID {uid} "));
                }
                self.out.raw("FLAGS ");
                fetch::write_flags(&mut self.out, &record.flags);
                self.out.raw(")").crlf();
            }
        }
        self.tagged(tag, "OK Store completed");
        Ok(())
    }

    /// Removes the messages marked deleted from the open mailbox, all of them or those in `only`.
    /// Says nothing to the client; a following [`sync`](Self::sync) does.
    async fn remove_deleted(&mut self, only: Option<&SequenceSet>) -> Outcome {
        let Some(selected) = self.selected.as_ref().filter(|selected| !selected.read_only) else { return Ok(()) };
        let mailbox = selected.mailbox.clone();
        let last_uid = selected.entries.last().map_or(0, |(uid, _)| uid.get());
        let records = self.records(None).await?.unwrap_or_default();
        let doomed: Vec<Uid> = records
            .iter()
            .filter(|(_, record)| record.flags.system.contains(SystemFlags::DELETED))
            .filter(|(_, record)| only.is_none_or(|set| search::in_set(set, record.uid.get(), last_uid)))
            .map(|(_, record)| record.uid)
            .collect();
        self.remove(&mailbox, &doomed).await
    }

    async fn remove(&mut self, mailbox: &Mailbox, uids: &[Uid]) -> Outcome {
        if uids.is_empty() {
            return Ok(());
        }
        let expunged = self.context.metadata.expunge(mailbox.id, uids).await?;
        self.discard(expunged.released_blobs).await;
        if !expunged.uids.is_empty() {
            self.publish(mailbox, ChangeKind::Expunged(expunged.uids)).await;
        }
        Ok(())
    }

    async fn expunge(&mut self, tag: &str, only: Option<&SequenceSet>) -> Outcome {
        if self.writable(tag).is_none() {
            return Ok(());
        }
        self.remove_deleted(only).await?;
        self.sync().await?;
        self.tagged(tag, "OK Expunge completed");
        Ok(())
    }

    async fn append(
        &mut self,
        tag: &str,
        account: AccountId,
        mailbox: &WireMailbox<'_>,
        flags: &[Flag<'_>],
        received: Option<SystemTime>,
        data: &[u8],
    ) -> Outcome {
        let Some(mailbox) = self.visible(account, mailbox).await? else {
            self.tagged(tag, "NO [TRYCREATE] No such mailbox");
            return Ok(());
        };
        let Some(flags) = flags_from(flags) else {
            self.tagged(tag, "BAD That flag cannot be stored");
            return Ok(());
        };
        if data.is_empty() {
            self.tagged(tag, "NO An empty message cannot be appended");
            return Ok(());
        }
        if data.len() as u64 > self.context.max_message_size {
            self.tagged(tag, "NO [TOOBIG] The message is too large");
            return Ok(());
        }
        let blob = self.context.blobs.put(Bytes::copy_from_slice(data)).await?;
        let new = NewMessage {
            blob,
            size: data.len() as u64,
            internal_date: received.unwrap_or_else(SystemTime::now),
            flags,
            once: None,
        };
        let record = match self.context.metadata.append_message(mailbox.id, new).await {
            Ok(record) => record,
            Err(error) => {
                self.discard(vec![blob]).await;
                return Err(error.into());
            }
        };
        self.publish(&mailbox, ChangeKind::Appended(record.uid)).await;
        if self.selected.as_ref().is_some_and(|selected| selected.mailbox.id == mailbox.id) {
            self.sync().await?;
        }
        self.tagged(tag, &format!("OK [APPENDUID {} {}] Append completed", mailbox.uid_validity.0, record.uid));
        Ok(())
    }

    /// COPY, and MOVE when `then_remove` is set.
    async fn copy(
        &mut self,
        tag: &str,
        account: AccountId,
        set: &SequenceSet,
        to: &WireMailbox<'_>,
        by_uid: bool,
        then_remove: bool,
    ) -> Outcome {
        let source = if then_remove {
            let Some(mailbox) = self.writable(tag) else { return Ok(()) };
            mailbox
        } else {
            let Some(selected) = &self.selected else {
                self.tagged(tag, "BAD No mailbox is selected");
                return Ok(());
            };
            selected.mailbox.clone()
        };
        let Some(target) = self.visible(account, to).await? else {
            self.tagged(tag, "NO [TRYCREATE] No such mailbox");
            return Ok(());
        };
        let records = self.records(Some((set, by_uid))).await?.unwrap_or_default();
        let (mut from, mut into) = (Vec::new(), Vec::new());
        for (_, record) in &records {
            // The copy points at the same stored message; nothing is duplicated.
            let new = NewMessage {
                blob: record.blob,
                size: record.size,
                internal_date: record.internal_date,
                flags: record.flags.clone(),
                once: None,
            };
            match self.context.metadata.append_message(target.id, new).await {
                Ok(copy) => {
                    from.push(record.uid);
                    into.push(copy.uid);
                    self.publish(&target, ChangeKind::Appended(copy.uid)).await;
                }
                // The original was removed, and its blob released, while this was under way.
                Err(Error::Conflict(_)) => continue,
                Err(error) => return Err(error.into()),
            }
        }
        let copied = format!("[COPYUID {} {} {}]", target.uid_validity.0, uid_set(&from), uid_set(&into));
        let verb = if then_remove { "Move" } else { "Copy" };
        if from.is_empty() {
            self.tagged(tag, &format!("OK {verb} completed, nothing to do"));
            return Ok(());
        }
        if then_remove {
            self.untagged(&format!("OK {copied} Moved"));
            self.remove(&source, &from).await?;
            self.sync().await?;
            self.tagged(tag, "OK Move completed");
        } else {
            if target.id == source.id {
                self.sync().await?;
            }
            self.tagged(tag, &format!("OK {copied} Copy completed"));
        }
        Ok(())
    }

    async fn search(
        &mut self,
        tag: &str,
        charset: Option<&Charset<'_>>,
        criteria: &[SearchKey<'_>],
        by_uid: bool,
    ) -> Outcome {
        let offered = |name: &str| ["US-ASCII", "UTF-8"].iter().any(|known| name.eq_ignore_ascii_case(known));
        let known = match charset {
            None => true,
            Some(Charset::Atom(name)) => offered(name.inner()),
            Some(Charset::Quoted(name)) => offered(name.inner()),
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

    #[test]
    fn uid_sets_collapse_runs() {
        let uids = |values: &[u32]| values.iter().map(|value| Uid::new(*value).unwrap()).collect::<Vec<_>>();
        assert_eq!(uid_set(&uids(&[3, 4, 5, 9, 11, 12])), "3:5,9,11:12");
        assert_eq!(uid_set(&uids(&[7])), "7");
        assert_eq!(uid_set(&uids(&[])), "");
    }
}
