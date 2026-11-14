// SPDX-FileCopyrightText: 2026 Sigmastone OÜ
// SPDX-License-Identifier: Apache-2.0

//! The submission server driven the way a mail client drives it, over an in-memory pipe.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use epistel_core::memory::{MemoryMetadataStore, MemoryOutboundRelay};
use epistel_core::model::{AccountState, NewAccount, NewCredential};
use epistel_core::{AccountId, AddressPattern, MetadataStore, RelayError, RelayLimits, directory, password};
use epistel_submission::Context;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

const LOGIN: &str = "ada@example.org";

struct Server {
    metadata: Arc<MemoryMetadataStore>,
    relay: Arc<MemoryOutboundRelay>,
    account: AccountId,
    secret: String,
}

struct Client {
    stream: DuplexStream,
    pending: Vec<u8>,
}

fn base64(text: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(text)
}

impl Server {
    async fn start() -> Self {
        Self::with_limits(RelayLimits { max_message_size: 1 << 20, max_recipients: 3 }).await
    }

    async fn with_limits(limits: RelayLimits) -> Self {
        let metadata = Arc::new(MemoryMetadataStore::new());
        let new = NewAccount { primary_address: LOGIN.parse().unwrap(), display_name: "Ada".into() };
        let account = directory::provision_account(&*metadata, new).await.unwrap().id;
        let secret = password::generate();
        let credential = NewCredential { label: "test".into(), secret_hash: password::hash(&secret) };
        metadata.add_credential(account, credential).await.unwrap();
        Self { metadata, relay: Arc::new(MemoryOutboundRelay::new(limits)), account, secret }
    }

    /// Connects a client and reads the greeting.
    async fn connect(&self) -> (Client, String) {
        let (near, far) = tokio::io::duplex(1 << 20);
        let context = Arc::new(Context {
            metadata: self.metadata.clone(),
            relay: self.relay.clone(),
            hostname: "mail.example.org".into(),
            max_message_size: 1 << 20,
            failed_login_delay: Duration::from_millis(10),
        });
        tokio::spawn(async move {
            let _ = epistel_submission::serve(far, context).await;
        });
        let mut client = Client { stream: near, pending: Vec::new() };
        let greeting = client.reply().await;
        (client, greeting)
    }

    /// Connects, says hello and logs in.
    async fn logged_in(&self) -> Client {
        let (mut client, _) = self.connect().await;
        client.run("EHLO client.example").await;
        let plain = base64(&format!("\0{LOGIN}\0{}", self.secret));
        assert_eq!(client.run(&format!("AUTH PLAIN {plain}")).await, "235 2.7.0 Authenticated");
        client
    }
}

impl Client {
    async fn send(&mut self, text: &str) {
        // The server may have said its last word and closed; what it said is still there to read.
        let _ = self.stream.write_all(text.as_bytes()).await;
    }

    /// The next reply, all its lines joined with `|`. Empty when the server closed.
    async fn reply(&mut self) -> String {
        let mut lines: Vec<String> = Vec::new();
        loop {
            if let Some(newline) = self.pending.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = self.pending.drain(..=newline).collect();
                let line = String::from_utf8_lossy(&line).trim_end().to_owned();
                // The last line of a reply has a space after the code, the others a hyphen.
                let last = line.as_bytes().get(3) != Some(&b'-');
                lines.push(line);
                if last {
                    return lines.join("|");
                }
                continue;
            }
            let mut chunk = [0u8; 4096];
            let read = tokio::time::timeout(Duration::from_secs(5), self.stream.read(&mut chunk));
            match read.await.expect("the server answers in time").unwrap() {
                0 => return lines.join("|"),
                count => self.pending.extend_from_slice(&chunk[..count]),
            }
        }
    }

    async fn run(&mut self, command: &str) -> String {
        self.send(&format!("{command}\r\n")).await;
        self.reply().await
    }

    /// Sends a whole message: sender, recipients, content. Returns the reply to the content.
    async fn submit(&mut self, sender: &str, recipients: &[&str], content: &str) -> String {
        assert!(self.run(&format!("MAIL FROM:<{sender}>")).await.starts_with("250"));
        for recipient in recipients {
            assert!(self.run(&format!("RCPT TO:<{recipient}>")).await.starts_with("250"));
        }
        assert!(self.run("DATA").await.starts_with("354"));
        self.run(&format!("{content}\r\n.")).await
    }
}

const MESSAGE: &str = "From: Ada <ada@example.org>\r\nTo: bob@elsewhere.example\r\nSubject: Hello\r\n\r\nHello Bob.";

#[tokio::test]
async fn the_server_introduces_itself_and_wants_a_login_first() {
    let server = Server::start().await;
    let (mut client, greeting) = server.connect().await;
    assert_eq!(greeting, "220 mail.example.org ESMTP Epistel");

    let hello = client.run("EHLO client.example").await;
    assert!(hello.starts_with("250-mail.example.org|250-SIZE 1048576|"), "{hello}");
    for extension in ["8BITMIME", "PIPELINING", "ENHANCEDSTATUSCODES", "SMTPUTF8", "250 AUTH PLAIN LOGIN"] {
        assert!(hello.contains(extension), "missing {extension} in {hello}");
    }
    assert!(!hello.contains("STARTTLS"));
    assert!(client.run("STARTTLS").await.starts_with("502 "));
    assert!(client.run("MAIL FROM:<ada@example.org>").await.starts_with("530 5.7.0"));
    assert!(client.run("RCPT TO:<bob@elsewhere.example>").await.starts_with("530 "));
    assert!(client.run("DATA").await.starts_with("530 "));
    assert_eq!(client.run("helo client.example").await, "250 mail.example.org");
    assert_eq!(client.run("noop").await, "250 2.0.0 OK");
    assert_eq!(client.run("QUIT").await, "221 2.0.0 Bye");
    assert_eq!(client.reply().await, "", "the server closes after QUIT");
}

#[tokio::test]
async fn logging_in_works_with_plain_and_with_login() {
    let server = Server::start().await;
    let plain = base64(&format!("\0{LOGIN}\0{}", server.secret));

    let (mut client, _) = server.connect().await;
    assert_eq!(client.run("AUTH PLAIN").await, "334", "an empty challenge");
    assert_eq!(client.run(&plain).await, "235 2.7.0 Authenticated");
    assert!(client.run(&format!("AUTH PLAIN {plain}")).await.starts_with("503 "));

    let (mut client, _) = server.connect().await;
    assert_eq!(client.run("AUTH LOGIN").await, "334 VXNlcm5hbWU6");
    assert_eq!(client.run(&base64(LOGIN)).await, "334 UGFzc3dvcmQ6");
    assert_eq!(client.run(&base64(&server.secret)).await, "235 2.7.0 Authenticated");

    let (mut client, _) = server.connect().await;
    assert_eq!(client.run(&format!("AUTH LOGIN {}", base64(LOGIN))).await, "334 UGFzc3dvcmQ6");
    assert!(client.run(&base64("wrong")).await.starts_with("535 5.7.8"));
    assert!(client.run(&format!("AUTH PLAIN {}", base64(&format!("\0{LOGIN}\0wrong")))).await.starts_with("535 "));
    assert!(
        client
            .run(&format!("AUTH PLAIN {}", base64(&format!("\0nobody@example.org\0{}", server.secret))))
            .await
            .starts_with("535 ")
    );
    assert!(
        client
            .run(&format!("AUTH PLAIN {}", base64(&format!("root\0{LOGIN}\0{}", server.secret))))
            .await
            .starts_with("535 ")
    );
    assert!(client.run("AUTH PLAIN !!!").await.starts_with("501 "));
    assert!(client.run("AUTH CRAM-MD5").await.starts_with("504 "));
    assert_eq!(client.run("AUTH PLAIN").await, "334", "an empty challenge");
    assert!(client.run("*").await.starts_with("501 5.7.0"));
    assert!(client.run("MAIL FROM:<ada@example.org>").await.starts_with("530 "), "still not logged in");

    server.metadata.set_account_state(server.account, AccountState::Suspended).await.unwrap();
    assert!(client.run(&format!("AUTH PLAIN {plain}")).await.starts_with("535 "), "a suspended account cannot send");
}

#[tokio::test]
async fn a_message_reaches_the_relay_completed_and_without_bcc() {
    let server = Server::start().await;
    let mut client = server.logged_in().await;
    let content = "From: Ada <ada@example.org>\r\nTo: bob@elsewhere.example\r\nBcc: carol@elsewhere.example\r\n\
Subject: Hello\r\n\r\nFirst line.\r\n..a line that began with a dot\r\nLast line.";

    let reply = client.submit("ada@example.org", &["bob@elsewhere.example", "Carol@Elsewhere.example"], content).await;
    assert_eq!(reply, "250 2.0.0 Accepted for delivery");

    let sent = server.relay.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].sender.as_str(), "ada@example.org");
    let recipients: Vec<_> = sent[0].recipients.iter().map(|address| address.as_str()).collect();
    assert_eq!(
        recipients,
        ["bob@elsewhere.example", "carol@elsewhere.example"],
        "the hidden recipient is in the envelope"
    );
    let raw = String::from_utf8_lossy(&sent[0].raw).into_owned();
    assert!(raw.starts_with("Date: ") && raw.contains("\r\nMessage-ID: <"), "{raw}");
    assert!(!raw.contains("Bcc") && !raw.contains("carol"), "and nowhere in the message: {raw}");
    assert!(
        raw.ends_with("Subject: Hello\r\n\r\nFirst line.\r\n.a line that began with a dot\r\nLast line.\r\n"),
        "{raw}"
    );

    // The envelope is used up; the next message starts from nothing.
    assert!(client.run("RCPT TO:<bob@elsewhere.example>").await.starts_with("503 "));
    assert!(client.submit("ada@example.org", &["bob@elsewhere.example"], MESSAGE).await.starts_with("250 "));
    assert_eq!(server.relay.sent().len(), 2);
}

#[tokio::test]
async fn a_user_sends_only_as_an_address_of_their_own() {
    let server = Server::start().await;
    let other = NewAccount { primary_address: "bob@example.org".parse().unwrap(), display_name: String::new() };
    server.metadata.create_account(other).await.unwrap();
    server.metadata.put_address(AddressPattern::parse("sales@example.org").unwrap(), server.account).await.unwrap();
    let mut client = server.logged_in().await;

    assert!(client.run("MAIL FROM:<bob@example.org>").await.starts_with("550 5.7.1"));
    assert!(client.run("MAIL FROM:<someone@elsewhere.example>").await.starts_with("550 5.7.1"));
    assert!(client.run("MAIL FROM:<>").await.starts_with("550 5.7.1"));
    assert!(client.run("MAIL FROM:<not an address>").await.starts_with("501 "));
    assert!(client.run("MAIL TO:<ada@example.org>").await.starts_with("501 "));

    let as_alias = MESSAGE.replace("ada@example.org", "Sales@example.org");
    assert!(client.submit("sales@example.org", &["x@elsewhere.example"], &as_alias).await.starts_with("250 "));

    // The envelope is the user's own, the header claims someone else.
    let forged = MESSAGE.replace("ada@example.org", "bob@example.org");
    assert!(
        client.submit("ada@example.org", &["x@elsewhere.example"], &forged).await.starts_with("550 5.7.1 The From")
    );
    let two_authors = MESSAGE.replace("From: Ada <ada@example.org>", "From: ada@example.org, bob@example.org");
    assert!(client.submit("ada@example.org", &["x@elsewhere.example"], &two_authors).await.starts_with("550 "));
    let no_author = "Subject: anonymous\r\n\r\nhello";
    assert!(client.submit("ada@example.org", &["x@elsewhere.example"], no_author).await.starts_with("554 5.6.0"));
    assert_eq!(server.relay.sent().len(), 1, "only the honest message went out");

    server.metadata.put_address(AddressPattern::parse("*@example.org").unwrap(), server.account).await.unwrap();
    assert!(client.run("MAIL FROM:<anything@example.org>").await.starts_with("250 "), "a catch-all covers sending too");
    client.run("RSET").await;
    assert!(client.run("MAIL FROM:<bob@example.org>").await.starts_with("550 "), "but not another account's address");
}

#[tokio::test]
async fn sizes_and_recipient_counts_are_limited() {
    let server = Server::with_limits(RelayLimits { max_message_size: 400, max_recipients: 2 }).await;
    let mut client = server.logged_in().await;

    assert!(
        client.run("EHLO again.example").await.contains("250-SIZE 400|"),
        "the smaller of the two limits is announced"
    );
    assert!(client.run("MAIL FROM:<ada@example.org> SIZE=401").await.starts_with("552 5.3.4"));
    assert!(client.run("MAIL FROM:<ada@example.org> SIZE=lots").await.starts_with("501 "));
    assert!(client.run("MAIL FROM:<ada@example.org> FUTURE=1").await.starts_with("555 "));
    assert!(client.run("MAIL FROM:<ada@example.org> SIZE=100 BODY=8BITMIME SMTPUTF8").await.starts_with("250 "));
    assert!(client.run("RCPT TO:<a@elsewhere.example>").await.starts_with("250 "));
    assert!(client.run("RCPT TO:<b@elsewhere.example>").await.starts_with("250 "));
    assert!(client.run("RCPT TO:<c@elsewhere.example>").await.starts_with("452 4.5.3"));
    assert!(client.run("RCPT TO:<nonsense>").await.starts_with("501 "));

    assert!(client.run("DATA").await.starts_with("354"));
    let long = format!("{MESSAGE}\r\n{}", "x".repeat(500));
    assert!(client.run(&format!("{long}\r\n.")).await.starts_with("552 5.3.4"));
    assert!(server.relay.sent().is_empty());
    assert!(
        client.submit("ada@example.org", &["a@elsewhere.example"], MESSAGE).await.starts_with("250 "),
        "and on it goes"
    );
}

#[tokio::test]
async fn what_the_relay_says_reaches_the_client() {
    let server = Server::start().await;
    let mut client = server.logged_in().await;

    server.relay.fail_next(RelayError::Rejected("the address is on the\r\nsuppression list".into()));
    let refused = client.submit("ada@example.org", &["x@elsewhere.example"], MESSAGE).await;
    assert_eq!(refused, "554 5.7.1 Refused: the address is on thesuppression list");

    server.relay.fail_next(RelayError::Transient("throttled".into()));
    assert!(client.submit("ada@example.org", &["x@elsewhere.example"], MESSAGE).await.starts_with("451 4.3.0"));

    assert!(client.submit("ada@example.org", &["x@elsewhere.example"], MESSAGE).await.starts_with("250 "));
    assert_eq!(server.relay.sent().len(), 1);
}

#[tokio::test]
async fn commands_sent_in_one_go_are_answered_in_order() {
    let server = Server::start().await;
    let mut client = server.logged_in().await;
    client
        .send(&format!(
            "MAIL FROM:<ada@example.org>\r\nRCPT TO:<a@elsewhere.example>\r\nRCPT TO:<b@elsewhere.example>\r\nDATA\r\n\
             {MESSAGE}\r\n.\r\nQUIT\r\n"
        ))
        .await;
    let mut replies = Vec::new();
    for _ in 0..6 {
        replies.push(client.reply().await[..3].to_owned());
    }
    assert_eq!(replies, ["250", "250", "250", "354", "250", "221"]);
    assert_eq!(server.relay.sent()[0].recipients.len(), 2);
}

#[tokio::test]
async fn out_of_order_and_unknown_commands_are_refused_and_too_many_end_the_conversation() {
    let server = Server::start().await;
    let mut client = server.logged_in().await;
    assert!(client.run("RCPT TO:<a@elsewhere.example>").await.starts_with("503 "));
    assert!(client.run("DATA").await.starts_with("503 "));
    assert!(client.run("MAIL FROM:<ada@example.org>").await.starts_with("250 "));
    assert!(client.run("MAIL FROM:<ada@example.org>").await.starts_with("503 "));
    assert!(client.run("DATA").await.starts_with("503 "), "no recipients yet");
    assert_eq!(client.run("RSET").await, "250 2.0.0 OK");
    assert!(client.run("RCPT TO:<a@elsewhere.example>").await.starts_with("503 "), "RSET forgets the sender");
    assert!(client.run("VRFY ada").await.starts_with("502 "));
    assert!(client.run("FROBNICATE").await.starts_with("500 "));
    assert!(client.run(&"X".repeat(5000)).await.starts_with("500 5.5.2 Line too long"));
    assert_eq!(client.run("NOOP").await, "250 2.0.0 OK");

    let mut last = String::new();
    for _ in 0..25 {
        last = client.run("FROBNICATE").await;
        if last.starts_with("421") {
            break;
        }
    }
    assert!(last.starts_with("421 4.7.0"), "{last}");
    assert_eq!(client.reply().await, "");
}
