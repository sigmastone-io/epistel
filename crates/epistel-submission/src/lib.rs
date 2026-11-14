// SPDX-FileCopyrightText: 2026 Sigmastone OÜ
// SPDX-License-Identifier: Apache-2.0

//! The mail submission server of Epistel: how a mail client hands over a message to be sent.
//!
//! SMTP as profiled for submission (RFC 6409), over a stream that is already inside TLS. Only a
//! logged-in user may send, only as an address that is theirs, and what is accepted is handed to
//! the outbound relay before the client is told so. There is no queue: when the relay cannot
//! take the message, the client keeps it and tries again.
//!
//! Extensions: `AUTH` (PLAIN and LOGIN), `SIZE`, `8BITMIME`, `PIPELINING`, `ENHANCEDSTATUSCODES`,
//! `SMTPUTF8`.

mod message;

use std::io;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use bytes::Bytes;
use epistel_core::{
    AccountId, EmailAddress, MetadataStore, OutboundMessage, OutboundRelay, RelayError, auth, directory,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tracing::{info, warn};

/// What a connection needs from the rest of the server.
pub struct Context {
    pub metadata: Arc<dyn MetadataStore>,
    pub relay: Arc<dyn OutboundRelay>,
    /// The server's own name, for the greeting and for message identifiers it has to make up.
    pub hostname: String,
    /// The largest message accepted, in bytes. The relay's own limit applies as well.
    pub max_message_size: u64,
    /// How long a failed login is held back before it is answered.
    pub failed_login_delay: Duration,
}

/// The longest command line accepted. The standard asks for 512; paths with parameters need more.
const LONGEST_LINE: usize = 4096;
/// How long the client may stay silent.
const PATIENCE: Duration = Duration::from_secs(5 * 60);
/// How many refused commands end the conversation.
const MOST_ERRORS: u32 = 20;

/// The stream, read a line at a time.
struct Lines<S> {
    stream: S,
    buffer: Vec<u8>,
}

enum Line {
    /// A line, without its line break.
    Text(Vec<u8>),
    /// A line longer than allowed. It was read to its end and thrown away.
    TooLong,
    /// The client is gone or fell silent.
    End,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Lines<S> {
    async fn next(&mut self, longest: usize) -> io::Result<Line> {
        let mut dropped = false;
        loop {
            if let Some(newline) = self.buffer.iter().position(|byte| *byte == b'\n') {
                let mut line: Vec<u8> = self.buffer.drain(..=newline).collect();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(if dropped || line.len() > longest { Line::TooLong } else { Line::Text(line) });
            }
            if self.buffer.len() > longest {
                // Keep reading to the line's end, without keeping the line.
                self.buffer.clear();
                dropped = true;
            }
            match tokio::time::timeout(PATIENCE, self.stream.read_buf(&mut self.buffer)).await {
                Ok(Ok(0)) | Err(_) => return Ok(Line::End),
                Ok(Ok(_)) => {}
                Ok(Err(error)) => return Err(error),
            }
        }
    }

    async fn reply(&mut self, text: &str) -> io::Result<()> {
        self.stream.write_all(text.as_bytes()).await?;
        self.stream.write_all(b"\r\n").await?;
        self.stream.flush().await
    }
}

/// The message being put together by MAIL, RCPT and DATA.
#[derive(Default)]
struct Envelope {
    sender: Option<EmailAddress>,
    recipients: Vec<EmailAddress>,
}

/// The address between the angle brackets of `FROM:<...>` or `TO:<...>`, and what follows it.
fn path(argument: &str) -> Option<(&str, &str)> {
    let argument = argument.trim_start();
    match argument.strip_prefix('<') {
        Some(rest) => rest.split_once('>'),
        // Not allowed by the standard, sent by some clients all the same.
        None => Some(argument.split_once(' ').unwrap_or((argument, ""))),
    }
}

fn decode(line: &[u8]) -> Option<Vec<u8>> {
    BASE64.decode(line.trim_ascii()).ok()
}

/// Serves one submission connection until the client quits, goes away or falls silent.
pub async fn serve<S>(stream: S, context: Arc<Context>) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut lines = Lines { stream, buffer: Vec::new() };
    let mut account: Option<AccountId> = None;
    let mut envelope = Envelope::default();
    let mut errors = 0u32;
    let limits = context.relay.limits();
    let size_limit = context.max_message_size.min(limits.max_message_size);
    let hostname = &context.hostname;
    lines.reply(&format!("220 {hostname} ESMTP Epistel")).await?;

    loop {
        if errors >= MOST_ERRORS {
            return lines.reply("421 4.7.0 Too many errors, closing").await;
        }
        let line = match lines.next(LONGEST_LINE).await? {
            Line::Text(line) => line,
            Line::TooLong => {
                errors += 1;
                lines.reply("500 5.5.2 Line too long").await?;
                continue;
            }
            Line::End => return Ok(()),
        };
        let Ok(line) = String::from_utf8(line) else {
            errors += 1;
            lines.reply("500 5.5.2 Commands are text").await?;
            continue;
        };
        let (verb, argument) = line.split_once(' ').unwrap_or((&line, ""));
        let verb = verb.to_ascii_uppercase();

        let reply: String = match verb.as_str() {
            "EHLO" => {
                envelope = Envelope::default();
                format!(
                    "250-{hostname}\r\n250-SIZE {size_limit}\r\n250-8BITMIME\r\n250-PIPELINING\r\n\
                     250-ENHANCEDSTATUSCODES\r\n250-SMTPUTF8\r\n250 AUTH PLAIN LOGIN"
                )
            }
            "HELO" => {
                envelope = Envelope::default();
                format!("250 {hostname}")
            }
            "NOOP" => "250 2.0.0 OK".to_owned(),
            "RSET" => {
                envelope = Envelope::default();
                "250 2.0.0 OK".to_owned()
            }
            "QUIT" => return lines.reply("221 2.0.0 Bye").await,
            // The connection is inside TLS from its first byte; there is nothing to upgrade.
            "STARTTLS" => "502 5.5.1 The connection is already encrypted".to_owned(),
            "AUTH" if account.is_some() => "503 5.5.1 Already authenticated".to_owned(),
            "AUTH" => {
                let (mechanism, initial) = argument.trim().split_once(' ').unwrap_or((argument.trim(), ""));
                let credentials = match mechanism.to_ascii_uppercase().as_str() {
                    "PLAIN" => read_plain(&mut lines, initial).await?,
                    "LOGIN" => read_login(&mut lines, initial).await?,
                    _ => Err("504 5.5.4 Only PLAIN and LOGIN are offered"),
                };
                match credentials {
                    Err(refusal) => refusal.to_owned(),
                    Ok((login, secret)) => match auth::check(&*context.metadata, &login, &secret).await {
                        Ok(Some(granted)) => {
                            account = Some(granted);
                            info!(account = %granted, "logged in");
                            "235 2.7.0 Authenticated".to_owned()
                        }
                        Ok(None) => {
                            tokio::time::sleep(context.failed_login_delay).await;
                            "535 5.7.8 Invalid credentials".to_owned()
                        }
                        Err(error) => {
                            warn!(%error, "a login failed on a backend");
                            "454 4.7.0 Try again later".to_owned()
                        }
                    },
                }
            }
            "MAIL" | "RCPT" | "DATA" if account.is_none() => "530 5.7.0 Authentication required".to_owned(),
            "MAIL" => match mail_from(&context, account, argument, size_limit, &envelope).await {
                Ok(sender) => {
                    envelope.sender = Some(sender);
                    "250 2.1.0 Sender OK".to_owned()
                }
                Err(refusal) => refusal,
            },
            "RCPT" => {
                let address = argument
                    .get(..3)
                    .filter(|keyword| keyword.eq_ignore_ascii_case("TO:"))
                    .and_then(|_| path(&argument[3..]))
                    .map(|(address, _)| EmailAddress::parse(address));
                match address {
                    _ if envelope.sender.is_none() => "503 5.5.1 Send MAIL first".to_owned(),
                    None => "501 5.5.4 Expected RCPT TO:<address>".to_owned(),
                    Some(Err(_)) => "501 5.1.3 That is not an address".to_owned(),
                    // The relay takes a limited number per message, and a message cannot be handed
                    // over in parts without risking that some recipients get it twice.
                    Some(Ok(_)) if envelope.recipients.len() >= limits.max_recipients => {
                        "452 4.5.3 Too many recipients, send the rest separately".to_owned()
                    }
                    Some(Ok(address)) => {
                        envelope.recipients.push(address);
                        "250 2.1.5 Recipient OK".to_owned()
                    }
                }
            }
            "DATA" if envelope.recipients.is_empty() => "503 5.5.1 Send MAIL and RCPT first".to_owned(),
            "DATA" => {
                lines.reply("354 End with a line holding only a dot").await?;
                let Some(received) = read_data(&mut lines, size_limit).await? else { return Ok(()) };
                let taken = std::mem::take(&mut envelope);
                match (received, taken.sender, account) {
                    (Some(raw), Some(sender), Some(account)) => {
                        send(&context, account, sender, taken.recipients, raw, size_limit).await
                    }
                    _ => "552 5.3.4 The message is too large".to_owned(),
                }
            }
            "VRFY" | "EXPN" | "HELP" | "ETRN" | "TURN" => "502 5.5.1 Not offered".to_owned(),
            _ => "500 5.5.2 Unknown command".to_owned(),
        };
        if reply.starts_with('5') {
            errors += 1;
        }
        lines.reply(&reply).await?;
    }
}

type Credentials = Result<(Vec<u8>, Vec<u8>), &'static str>;

const CANCELLED: &str = "501 5.7.0 Authentication cancelled";
const NOT_BASE64: &str = "501 5.5.2 That is not base64";

/// Asks for one line of the authentication exchange. `None` when the client went away.
async fn challenge<S>(lines: &mut Lines<S>, prompt: &str) -> io::Result<Option<Result<Vec<u8>, &'static str>>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    lines.reply(&format!("334 {prompt}")).await?;
    Ok(match lines.next(LONGEST_LINE).await? {
        Line::Text(line) if line == b"*" => Some(Err(CANCELLED)),
        Line::Text(line) => Some(decode(&line).ok_or(NOT_BASE64)),
        Line::TooLong => Some(Err(NOT_BASE64)),
        Line::End => None,
    })
}

/// SASL PLAIN: one response holding user name and password.
async fn read_plain<S>(lines: &mut Lines<S>, initial: &str) -> io::Result<Credentials>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let response = match initial {
        "" => match challenge(lines, "").await? {
            Some(response) => response,
            None => return Ok(Err(CANCELLED)),
        },
        // A lone equals sign stands for an empty response.
        "=" => Ok(Vec::new()),
        initial => decode(initial.as_bytes()).ok_or(NOT_BASE64),
    };
    Ok(response.and_then(|response| match auth::split_plain(&response) {
        Some((authorize_as, login, _)) if !authorize_as.is_empty() && authorize_as != login => {
            Err("535 5.7.8 Acting for another user is not offered")
        }
        Some((_, login, secret)) => Ok((login.to_vec(), secret.to_vec())),
        None => Err("501 5.5.2 The PLAIN response is malformed"),
    }))
}

/// The LOGIN mechanism: the same user name and password, asked for one after the other. Not a
/// standard, but the only one some widely used clients speak. The prompts are the fixed words
/// "Username:" and "Password:" in base64.
async fn read_login<S>(lines: &mut Lines<S>, initial: &str) -> io::Result<Credentials>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let login = match initial {
        "" => match challenge(lines, "VXNlcm5hbWU6").await? {
            Some(login) => login,
            None => return Ok(Err(CANCELLED)),
        },
        initial => decode(initial.as_bytes()).ok_or(NOT_BASE64),
    };
    let login = match login {
        Ok(login) => login,
        Err(refusal) => return Ok(Err(refusal)),
    };
    Ok(match challenge(lines, "UGFzc3dvcmQ6").await? {
        Some(Ok(secret)) => Ok((login, secret)),
        Some(Err(refusal)) => Err(refusal),
        None => Err(CANCELLED),
    })
}

/// Checks `MAIL FROM:<address> [parameters]`. The address has to be one of the user's own.
async fn mail_from(
    context: &Context,
    account: Option<AccountId>,
    argument: &str,
    size_limit: u64,
    envelope: &Envelope,
) -> Result<EmailAddress, String> {
    if envelope.sender.is_some() {
        return Err("503 5.5.1 A message is already under way; RSET to start over".to_owned());
    }
    let after_keyword =
        argument.get(..5).filter(|keyword| keyword.eq_ignore_ascii_case("FROM:")).map(|_| &argument[5..]);
    let Some((address, parameters)) = after_keyword.and_then(path) else {
        return Err("501 5.5.4 Expected MAIL FROM:<address>".to_owned());
    };
    for parameter in parameters.split_ascii_whitespace() {
        let (name, value) = parameter.split_once('=').unwrap_or((parameter, ""));
        match name.to_ascii_uppercase().as_str() {
            "SIZE" => match value.parse::<u64>() {
                Ok(size) if size > size_limit => return Err("552 5.3.4 The message is too large".to_owned()),
                Ok(_) => {}
                Err(_) => return Err("501 5.5.4 SIZE takes a number".to_owned()),
            },
            "BODY" if ["7BIT", "8BITMIME"].iter().any(|known| value.eq_ignore_ascii_case(known)) => {}
            // Addresses and headers are passed on as they come, whatever they are written in.
            "SMTPUTF8" => {}
            // Who originally submitted the message. Nothing here acts on it.
            "AUTH" => {}
            _ => return Err(format!("555 5.5.4 The parameter {name} is not offered")),
        }
    }
    if address.is_empty() {
        return Err("550 5.7.1 A message needs a sender here".to_owned());
    }
    let Ok(address) = EmailAddress::parse(address) else {
        return Err("501 5.1.7 That is not an address".to_owned());
    };
    let account = account.ok_or("530 5.7.0 Authentication required")?;
    match directory::owns(&*context.metadata, account, &address).await {
        Ok(true) => Ok(address),
        Ok(false) => Err("550 5.7.1 That sender address is not yours".to_owned()),
        Err(error) => {
            warn!(%error, "could not check a sender address");
            Err("451 4.3.0 Try again later".to_owned())
        }
    }
}

/// Reads the message after DATA, up to the line holding only a dot.
///
/// The outer `None` means the client went away; the inner one that the message outgrew the limit
/// and was read to its end without being kept.
async fn read_data<S>(lines: &mut Lines<S>, size_limit: u64) -> io::Result<Option<Option<Vec<u8>>>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let limit = usize::try_from(size_limit).unwrap_or(usize::MAX);
    let mut message = Vec::new();
    let mut too_large = false;
    loop {
        let line = match lines.next(limit).await? {
            Line::Text(line) => line,
            Line::TooLong => {
                too_large = true;
                continue;
            }
            Line::End => return Ok(None),
        };
        if line == b"." {
            return Ok(Some((!too_large).then_some(message)));
        }
        // A line that begins with a dot was sent with one more, so that it is not taken for the end.
        let content = line.strip_prefix(b".").unwrap_or(&line);
        if too_large || message.len() + content.len() + 2 > limit {
            too_large = true;
            message = Vec::new();
            continue;
        }
        message.extend_from_slice(content);
        message.extend_from_slice(b"\r\n");
    }
}

/// Checks the message itself, completes it, and hands it to the relay. Returns the reply.
async fn send(
    context: &Context,
    account: AccountId,
    sender: EmailAddress,
    recipients: Vec<EmailAddress>,
    raw: Vec<u8>,
    size_limit: u64,
) -> String {
    let authors = match message::authors(&raw) {
        Some(authors) => authors,
        None => return "554 5.6.0 The message needs a From header with an address".to_owned(),
    };
    // The address the recipient sees has to be the user's own too, not only the envelope's.
    for author in &authors {
        match directory::owns(&*context.metadata, account, author).await {
            Ok(true) => {}
            Ok(false) => return "550 5.7.1 The From address is not yours".to_owned(),
            Err(error) => {
                warn!(%error, "could not check a From address");
                return "451 4.3.0 Try again later".to_owned();
            }
        }
    }
    let raw = message::prepare(&raw, &context.hostname);
    if raw.len() as u64 > size_limit {
        return "552 5.3.4 The message is too large".to_owned();
    }
    let count = recipients.len();
    let size = raw.len();
    match context.relay.send(OutboundMessage { sender, recipients, raw: Bytes::from(raw) }).await {
        Ok(()) => {
            info!(%account, recipients = count, size, "message handed to the relay");
            "250 2.0.0 Accepted for delivery".to_owned()
        }
        Err(RelayError::TooLarge) => "552 5.3.4 The message is too large".to_owned(),
        Err(RelayError::RecipientCount) => "452 4.5.3 Too many recipients, send the rest separately".to_owned(),
        Err(RelayError::Rejected(reason)) => {
            info!(%account, reason, "the relay refused a message");
            // The reason goes back on one line, without anything that could end the reply early.
            let reason: String = reason.chars().filter(|c| !c.is_control()).take(200).collect();
            format!("554 5.7.1 Refused: {reason}")
        }
        Err(RelayError::Transient(error)) => {
            warn!(%account, %error, "the relay could not take a message");
            "451 4.3.0 The message could not be passed on, try again later".to_owned()
        }
    }
}
