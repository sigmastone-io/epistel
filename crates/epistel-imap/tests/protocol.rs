//! The IMAP server driven the way a client drives it: text in, text out, over an in-memory pipe.

use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use base64::Engine;
use bytes::Bytes;
use epistel_core::memory::{MemoryBlobStore, MemoryMetadataStore};
use epistel_core::model::{AccountState, Flags, MailboxRole, NewAccount, NewCredential, NewMessage, SystemFlags};
use epistel_core::{AccountId, BlobStore, MailboxId, MetadataStore, directory, password};
use epistel_imap::Context;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

const LOGIN: &str = "ada@example.org";

const PLAIN: &[u8] = b"Date: Mon, 5 Jan 2026 10:00:00 +0000\r\nFrom: Bob <bob@example.org>\r\nTo: ada@example.org\r\n\
Subject: First message\r\nMessage-ID: <1@example.org>\r\n\r\nHello Ada.\r\n";

const MIXED: &[u8] = b"Date: Tue, 6 Jan 2026 11:00:00 +0000\r\nFrom: carol@example.org\r\nTo: ada@example.org\r\n\
Subject: Report attached\r\nContent-Type: multipart/mixed; boundary=xx\r\n\r\n--xx\r\n\
Content-Type: text/plain\r\n\r\nSee attachment.\r\n--xx\r\n\
Content-Type: application/pdf; name=\"r.pdf\"\r\nContent-Transfer-Encoding: base64\r\n\r\nQUJD\r\n--xx--\r\n";

struct Server {
    metadata: Arc<MemoryMetadataStore>,
    blobs: Arc<MemoryBlobStore>,
    account: AccountId,
    inbox: MailboxId,
    secret: String,
}

struct Client {
    stream: DuplexStream,
    pending: Vec<u8>,
    /// Set once the server has closed the connection.
    closed: bool,
}

impl Server {
    /// An account with two messages in its inbox, the first one read.
    async fn start() -> Self {
        let metadata = Arc::new(MemoryMetadataStore::new());
        let blobs = Arc::new(MemoryBlobStore::new());
        let new = NewAccount { primary_address: LOGIN.parse().unwrap(), display_name: "Ada".into() };
        let account = directory::provision_account(&*metadata, new).await.unwrap().id;
        let secret = password::generate();
        let credential = NewCredential { label: "test".into(), secret_hash: password::hash(&secret) };
        metadata.add_credential(account, credential).await.unwrap();
        let inbox = directory::mailbox_with_role(&*metadata, account, MailboxRole::Inbox).await.unwrap().unwrap().id;
        let server = Self { metadata, blobs, account, inbox, secret };
        server.deliver(PLAIN, SystemFlags::SEEN).await;
        server.deliver(MIXED, SystemFlags::empty()).await;
        server
    }

    async fn deliver(&self, raw: &'static [u8], flags: SystemFlags) {
        let blob = self.blobs.put(Bytes::from_static(raw)).await.unwrap();
        let new = NewMessage {
            blob,
            size: raw.len() as u64,
            internal_date: UNIX_EPOCH + Duration::from_secs(1_767_607_200),
            flags: Flags::from(flags),
            once: None,
        };
        self.metadata.append_message(self.inbox, new).await.unwrap();
    }

    /// Connects a client and reads the greeting.
    async fn connect(&self) -> (Client, String) {
        let (near, far) = tokio::io::duplex(1 << 20);
        let context = Arc::new(Context {
            metadata: self.metadata.clone(),
            blobs: self.blobs.clone(),
            failed_login_delay: Duration::from_millis(10),
        });
        tokio::spawn(async move {
            let _ = epistel_imap::serve(far, context).await;
        });
        let mut client = Client { stream: near, pending: Vec::new(), closed: false };
        let greeting = client.line().await;
        (client, greeting)
    }

    /// Connects, logs in and opens the inbox.
    async fn open_inbox(&self) -> Client {
        let (mut client, _) = self.connect().await;
        assert!(client.run(&format!("LOGIN {LOGIN} {}", self.secret)).await.contains("a OK"));
        assert!(client.run("SELECT INBOX").await.contains("a OK [READ-ONLY]"));
        client
    }
}

impl Client {
    async fn send(&mut self, text: &str) {
        self.stream.write_all(text.as_bytes()).await.unwrap();
    }

    /// The next line from the server, without its line break. Empty at the end of the connection.
    async fn line(&mut self) -> String {
        loop {
            if let Some(newline) = self.pending.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = self.pending.drain(..=newline).collect();
                return String::from_utf8_lossy(&line).trim_end().to_owned();
            }
            let mut chunk = [0u8; 4096];
            let read = tokio::time::timeout(Duration::from_secs(5), self.stream.read(&mut chunk));
            match read.await.expect("the server answers in time").unwrap() {
                0 => {
                    self.closed = true;
                    return String::new();
                }
                count => self.pending.extend_from_slice(&chunk[..count]),
            }
        }
    }

    /// Sends a command under the tag `a` and returns everything up to and including its answer.
    async fn run(&mut self, command: &str) -> String {
        self.send(&format!("a {command}\r\n")).await;
        self.until_tagged().await
    }

    async fn until_tagged(&mut self) -> String {
        let mut response = String::new();
        loop {
            let line = self.line().await;
            assert!(!self.closed || !response.is_empty(), "the connection closed without an answer");
            let done = line.starts_with("a ") || self.closed;
            response.push_str(&line);
            response.push('\n');
            if done {
                return response;
            }
        }
    }
}

#[tokio::test]
async fn the_greeting_announces_what_is_implemented_and_nothing_works_before_login() {
    let server = Server::start().await;
    let (mut client, greeting) = server.connect().await;
    assert!(greeting.starts_with("* OK [CAPABILITY IMAP4rev1 AUTH=PLAIN "), "{greeting}");
    assert!(!greeting.contains("STARTTLS") && !greeting.contains("LOGINDISABLED") && !greeting.contains("IDLE"));

    assert!(client.run("SELECT INBOX").await.contains("a BAD Log in first"));
    assert!(client.run("LIST \"\" *").await.contains("a BAD"));
    let capability = client.run("CAPABILITY").await;
    assert!(capability.contains("* CAPABILITY IMAP4rev1") && capability.contains("a OK"));
    assert!(client.run("NOOP").await.contains("a OK"));
    assert!(client.run("ID (\"name\" \"test\")").await.contains("* ID (\"name\" \"Epistel\""));
    let goodbye = client.run("LOGOUT").await;
    assert!(goodbye.contains("* BYE") && goodbye.contains("a OK"));
    assert_eq!(client.line().await, "", "the server closes after LOGOUT");
}

#[tokio::test]
async fn logging_in_takes_the_app_password_and_nothing_else() {
    let server = Server::start().await;
    let (mut client, _) = server.connect().await;
    assert!(client.run(&format!("LOGIN {LOGIN} wrong-password")).await.contains("a NO [AUTHENTICATIONFAILED]"));
    assert!(
        client
            .run(&format!("LOGIN nobody@example.org {}", server.secret))
            .await
            .contains("a NO [AUTHENTICATIONFAILED]")
    );
    assert!(client.run("LOGIN not-an-address x").await.contains("a NO [AUTHENTICATIONFAILED]"));
    assert!(client.run("AUTHENTICATE CRAM-MD5").await.contains("a NO"));

    let granted = client.run(&format!("LOGIN {LOGIN} {}", server.secret)).await;
    assert!(granted.contains("a OK [CAPABILITY "), "{granted}");
    assert!(client.run(&format!("LOGIN {LOGIN} {}", server.secret)).await.contains("a BAD Already logged in"));

    let used = server.metadata.list_credentials(server.account).await.unwrap();
    assert!(used[0].last_used.is_some(), "a login records when the password was used");
}

#[tokio::test]
async fn sasl_plain_works_with_and_without_an_initial_response() {
    let server = Server::start().await;
    let encode = |text: String| base64::engine::general_purpose::STANDARD.encode(text);
    let good = encode(format!("\0{LOGIN}\0{}", server.secret));

    let (mut client, _) = server.connect().await;
    assert!(client.run(&format!("AUTHENTICATE PLAIN {good}")).await.contains("a OK"));

    let (mut client, _) = server.connect().await;
    client.send("a AUTHENTICATE PLAIN\r\n").await;
    assert_eq!(client.line().await, "+");
    client.send(&format!("{good}\r\n")).await;
    assert!(client.until_tagged().await.contains("a OK"));

    let (mut client, _) = server.connect().await;
    client.send("a AUTHENTICATE PLAIN\r\n").await;
    client.line().await;
    client.send("*\r\n").await;
    assert!(client.until_tagged().await.contains("a BAD"));
    let wrong = encode(format!("\0{LOGIN}\0nope"));
    assert!(client.run(&format!("AUTHENTICATE PLAIN {wrong}")).await.contains("a NO [AUTHENTICATIONFAILED]"));
    let for_other = encode(format!("root@example.org\0{LOGIN}\0{}", server.secret));
    assert!(client.run(&format!("AUTHENTICATE PLAIN {for_other}")).await.contains("a NO [AUTHORIZATIONFAILED]"));
}

#[tokio::test]
async fn a_suspended_account_cannot_log_in() {
    let server = Server::start().await;
    server.metadata.set_account_state(server.account, AccountState::Suspended).await.unwrap();
    let (mut client, _) = server.connect().await;
    assert!(client.run(&format!("LOGIN {LOGIN} {}", server.secret)).await.contains("a NO [AUTHENTICATIONFAILED]"));
}

#[tokio::test]
async fn mailboxes_are_listed_with_their_roles_and_the_quarantine_is_hidden() {
    let server = Server::start().await;
    let (mut client, _) = server.connect().await;
    client.run(&format!("LOGIN {LOGIN} {}", server.secret)).await;

    let all = client.run("LIST \"\" \"*\"").await;
    assert!(all.contains("* LIST (\\HasNoChildren) \"/\" \"INBOX\""), "{all}");
    assert!(all.contains("* LIST (\\HasNoChildren \\Sent) \"/\" \"Sent\""));
    assert!(all.contains("\\Junk") && all.contains("\\Trash") && all.contains("\\Drafts") && all.contains("\\Archive"));
    assert!(!all.contains("Quarantine"));
    assert_eq!(all.matches("* LIST").count(), 6);

    assert!(client.run("LIST \"\" \"\"").await.contains("* LIST (\\Noselect) \"/\" \"\""));
    assert!(client.run("LIST \"\" inbox").await.contains("\"INBOX\""), "INBOX matches in any case");
    assert_eq!(client.run("LIST \"\" S%").await.matches("* LIST").count(), 1);
    assert_eq!(client.run("LSUB \"\" *").await.matches("* LSUB").count(), 6);
    assert!(client.run("NAMESPACE").await.contains("* NAMESPACE ((\"\" \"/\")) NIL NIL"));

    let status = client.run("STATUS INBOX (MESSAGES UNSEEN UIDNEXT UIDVALIDITY RECENT)").await;
    assert!(status.contains("* STATUS \"INBOX\" (MESSAGES 2 UNSEEN 1 UIDNEXT 3 UIDVALIDITY "), "{status}");
    assert!(client.run("STATUS Nowhere (MESSAGES)").await.contains("a NO [NONEXISTENT]"));
    assert!(client.run("STATUS Quarantine (MESSAGES)").await.contains("a NO [NONEXISTENT]"));
}

#[tokio::test]
async fn opening_a_mailbox_describes_it_and_is_read_only() {
    let server = Server::start().await;
    let (mut client, _) = server.connect().await;
    client.run(&format!("LOGIN {LOGIN} {}", server.secret)).await;

    assert!(client.run("FETCH 1 FLAGS").await.contains("a BAD No mailbox is selected"));
    let selected = client.run("SELECT inbox").await;
    for expected in
        ["* 2 EXISTS", "* 0 RECENT", "* FLAGS (", "[PERMANENTFLAGS ()]", "[UNSEEN 2]", "[UIDVALIDITY ", "[UIDNEXT 3]"]
    {
        assert!(selected.contains(expected), "missing {expected} in {selected}");
    }
    assert!(selected.contains("a OK [READ-ONLY]"));
    assert!(client.run("EXAMINE Sent").await.contains("* 0 EXISTS"));
    assert!(client.run("SELECT Nowhere").await.contains("a NO [NONEXISTENT]"));
    assert!(client.run("FETCH 1 FLAGS").await.contains("a BAD"), "a failed SELECT leaves nothing selected");
    assert!(client.run("SELECT Quarantine").await.contains("a NO"));

    for refused in ["CREATE Work", "DELETE Sent", "STORE 1 +FLAGS (\\Seen)", "EXPUNGE", "COPY 1 Sent", "SUBSCRIBE Sent"]
    {
        assert!(client.run(refused).await.contains("a NO [CANNOT]"), "{refused}");
    }
    assert!(client.run("IDLE").await.contains("a BAD"));
}

#[tokio::test]
async fn fetch_returns_record_data_envelope_and_structure() {
    let server = Server::start().await;
    let mut client = server.open_inbox().await;

    let basics = client.run("FETCH 1:* (UID FLAGS RFC822.SIZE INTERNALDATE)").await;
    let first = format!(
        "* 1 FETCH (UID 1 FLAGS (\\Seen) RFC822.SIZE {} INTERNALDATE \"05-Jan-2026 10:00:00 +0000\")",
        PLAIN.len()
    );
    assert!(basics.contains(&first), "{basics}");
    assert!(basics.contains(&format!("* 2 FETCH (UID 2 FLAGS () RFC822.SIZE {}", MIXED.len())));

    let fast = client.run("FETCH 2 FAST").await;
    assert!(fast.contains("* 2 FETCH (FLAGS () INTERNALDATE ") && fast.contains("RFC822.SIZE"));
    let by_uid = client.run("UID FETCH 2 FLAGS").await;
    assert!(by_uid.contains("* 2 FETCH (UID 2 FLAGS ())"), "a fetch by UID reports the UID: {by_uid}");
    assert_eq!(client.run("UID FETCH 5:9 FLAGS").await.matches("FETCH (").count(), 0);
    assert_eq!(client.run("UID FETCH 2:* FLAGS").await.matches("FETCH (").count(), 1);

    let envelope = client.run("FETCH 1 ENVELOPE").await;
    assert!(
        envelope.contains(
            r#"ENVELOPE ("Mon, 5 Jan 2026 10:00:00 +0000" "First message" (("Bob" NIL "bob" "example.org"))"#
        ),
        "{envelope}"
    );
    assert!(envelope.contains(r#"((NIL NIL "ada" "example.org")) NIL NIL NIL "<1@example.org>"))"#));

    let structure = client.run("FETCH 2 BODYSTRUCTURE").await;
    assert!(
        structure
            .contains(r#"BODYSTRUCTURE (("TEXT" "PLAIN" ("CHARSET" "US-ASCII") NIL NIL "7BIT" 15 1 NIL NIL NIL NIL)"#),
        "{structure}"
    );
    assert!(structure.contains(r#"("APPLICATION" "PDF" ("NAME" "r.pdf") NIL NIL "BASE64" 4 NIL NIL NIL NIL) "MIXED" ("BOUNDARY" "xx") NIL NIL NIL)"#));
    assert!(
        client
            .run("FETCH 1 BODY")
            .await
            .contains(r#"BODY ("TEXT" "PLAIN" ("CHARSET" "US-ASCII") NIL NIL "7BIT" 12 1)"#)
    );
    assert!(client.run("FETCH 1 BINARY[1]").await.contains("a NO [CANNOT]"));
}

#[tokio::test]
async fn fetch_returns_sections_as_literals_and_leaves_messages_unread() {
    let server = Server::start().await;
    let mut client = server.open_inbox().await;

    let whole = client.run("FETCH 1 BODY[]").await;
    assert!(whole.contains(&format!("* 1 FETCH (BODY[] {{{}}}", PLAIN.len())), "{whole}");
    assert!(whole.contains("Hello Ada."));
    let fields = client.run("UID FETCH 2 (BODY.PEEK[HEADER.FIELDS (Subject From)])").await;
    assert!(fields.contains("BODY[HEADER.FIELDS (SUBJECT FROM)] {53}"), "{fields}");
    assert!(fields.contains("From: carol@example.org\nSubject: Report attached\n"));
    assert!(client.run("FETCH 2 BODY[1]").await.contains("BODY[1] {15}\nSee attachment."));
    assert!(client.run("FETCH 2 BODY[2]").await.contains("BODY[2] {4}\nQUJD"));
    assert!(client.run("FETCH 2 BODY[2.MIME]").await.contains("Content-Transfer-Encoding: base64"));
    assert!(client.run("FETCH 2 BODY[7]").await.contains("BODY[7] NIL"));
    assert!(client.run("FETCH 1 BODY[TEXT]<6.3>").await.contains("BODY[TEXT]<6> {3}\nAda"));
    assert!(client.run("FETCH 1 BODY[TEXT]<500.3>").await.contains("BODY[TEXT]<500> {0}"));
    assert!(client.run("FETCH 1 RFC822.HEADER").await.contains("RFC822.HEADER {"));
    assert!(client.run("FETCH 1 RFC822.TEXT").await.contains("RFC822.TEXT {12}\nHello Ada."));

    assert!(client.run("FETCH 2 BODY[]").await.contains("a OK"));
    assert!(client.run("FETCH 2 FLAGS").await.contains("FLAGS ()"), "reading a read-only mailbox marks nothing");
}

#[tokio::test]
async fn search_finds_by_flags_text_and_sets() {
    let server = Server::start().await;
    let mut client = server.open_inbox().await;

    assert!(client.run("SEARCH ALL").await.contains("* SEARCH 1 2\n"));
    assert!(client.run("SEARCH UNSEEN").await.contains("* SEARCH 2\n"));
    assert!(client.run("SEARCH SEEN SUBJECT first").await.contains("* SEARCH 1\n"));
    assert!(client.run("SEARCH FROM carol").await.contains("* SEARCH 2\n"));
    assert!(client.run("SEARCH BODY \"see attachment\"").await.contains("* SEARCH 2\n"));
    assert!(client.run("SEARCH OR SUBJECT first SUBJECT report").await.contains("* SEARCH 1 2\n"));
    assert!(client.run("SEARCH SUBJECT nothing-like-this").await.contains("* SEARCH\n"));
    assert!(client.run("UID SEARCH 2:* NOT SEEN").await.contains("* SEARCH 2\n"));
    assert!(client.run("SEARCH CHARSET UTF-8 TEXT hello").await.contains("* SEARCH 1\n"));
    assert!(client.run("SEARCH CHARSET KOI8-R TEXT hello").await.contains("a NO [BADCHARSET"));
    assert!(client.run("SEARCH SENTON 6-Jan-2026").await.contains("* SEARCH 2\n"));
}

#[tokio::test]
async fn new_mail_is_announced_on_the_next_check() {
    let server = Server::start().await;
    let mut client = server.open_inbox().await;
    assert!(!client.run("NOOP").await.contains("EXISTS"));

    server.deliver(PLAIN, SystemFlags::empty()).await;
    assert!(client.run("NOOP").await.contains("* 3 EXISTS"));
    assert!(!client.run("NOOP").await.contains("EXISTS"), "announced once");
    assert!(client.run("FETCH 3 UID").await.contains("* 3 FETCH (UID 3)"));
}

#[tokio::test]
async fn literals_are_accepted_in_both_forms_and_refused_when_too_long() {
    let server = Server::start().await;

    let (mut client, _) = server.connect().await;
    client.send(&format!("a LOGIN {{{}}}\r\n", LOGIN.len())).await;
    assert_eq!(client.line().await, "+ OK");
    client.send(&format!("{LOGIN} {{{}}}\r\n", server.secret.len())).await;
    assert_eq!(client.line().await, "+ OK");
    client.send(&format!("{}\r\n", server.secret)).await;
    assert!(client.until_tagged().await.contains("a OK"));

    let (mut client, _) = server.connect().await;
    client
        .send(&format!("a LOGIN {{{}+}}\r\n{LOGIN} {{{}+}}\r\n{}\r\n", LOGIN.len(), server.secret.len(), server.secret))
        .await;
    assert!(client.until_tagged().await.contains("a OK"), "a non-synchronizing literal needs no go-ahead");

    let (mut client, _) = server.connect().await;
    client.send("a LOGIN {10000000}\r\n").await;
    assert!(client.until_tagged().await.contains("a BAD"));
    assert!(client.line().await.starts_with("* BYE"));
    assert_eq!(client.line().await, "");
}

#[tokio::test]
async fn nonsense_gets_an_answer_and_the_conversation_goes_on() {
    let server = Server::start().await;
    let (mut client, _) = server.connect().await;
    assert!(client.run("FROBNICATE now").await.contains("a BAD"));
    client.send("(((\r\n").await;
    assert!(client.line().await.starts_with("* BAD"));
    assert!(client.run("NOOP").await.contains("a OK"));
    assert!(client.run(&format!("LOGIN {LOGIN} {}", server.secret)).await.contains("a OK"));
    assert!(client.run("ENABLE CONDSTORE").await.contains("* ENABLED\n"));
    assert!(client.run("UNSELECT").await.contains("a BAD No mailbox is selected"));
    client.run("SELECT INBOX").await;
    assert!(client.run("CLOSE").await.contains("a OK"));
    assert!(client.run("SEARCH ALL").await.contains("a BAD No mailbox is selected"));
}
