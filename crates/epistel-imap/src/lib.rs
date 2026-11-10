//! The IMAP server of Epistel.
//!
//! [`serve`] runs one connection over any byte stream. Transport security is the caller's
//! business: the server hands this crate a stream that is already inside TLS, and the tests hand
//! it an in-memory pipe.
//!
//! IMAP4rev1 with the extensions the greeting names: logging in, managing mailboxes, reading,
//! searching, changing and moving messages, and waiting for news with `IDLE`.

pub mod fetch;
pub mod search;
mod session;
pub mod wire;

use std::io;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use imap_codec::CommandCodec;
use imap_codec::decode::{CommandDecodeError, Decoder};
use imap_codec::imap_types::core::LiteralMode;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tracing::debug;

pub(crate) use epistel_core::{header_address as address, mime};
pub use session::{CAPABILITIES, Context};
use session::{Next, Session};

/// The longest command accepted, not counting a message being appended.
const LONGEST_COMMAND: usize = 64 * 1024;
/// How often an idling client hears from the server, so that nothing between them drops the
/// connection for being quiet.
const IDLE_KEEPALIVE: Duration = Duration::from_secs(120);
/// How long a connection may stay silent before it has logged in.
const PATIENCE_BEFORE_LOGIN: Duration = Duration::from_secs(60);
/// How long a logged-in connection may stay silent. The standard asks for at least 30 minutes.
const PATIENCE_AFTER_LOGIN: Duration = Duration::from_secs(31 * 60);

/// The tag of a command line that could not be parsed, if it has a usable one.
fn tag_of(line: &[u8]) -> Option<&str> {
    let tag = line.split(|byte| *byte == b' ').next()?;
    let usable = !tag.is_empty() && tag.iter().all(|byte| byte.is_ascii_graphic() && !b"(){%*\"\\+".contains(byte));
    if usable { std::str::from_utf8(tag).ok() } else { None }
}

/// Takes the first line out of the buffer, without its line break.
fn take_line(buffer: &mut Vec<u8>) -> Option<Vec<u8>> {
    let newline = buffer.iter().position(|byte| *byte == b'\n')?;
    let mut line: Vec<u8> = buffer.drain(..=newline).collect();
    line.pop();
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    Some(line)
}

/// Waits with an idling client: passes on what happens in its mailbox, keeps the connection
/// alive, and ends when the client says `DONE`. `false` when the client went away instead.
async fn idle<R, W>(session: &mut Session<W>, reader: &mut R, buffer: &mut Vec<u8>, tag: &str) -> io::Result<bool>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut watching = session.watch().await;
    loop {
        if let Some(line) = take_line(buffer) {
            let done = line.eq_ignore_ascii_case(b"DONE");
            let answer = if done { "OK Idle completed" } else { "BAD Expected DONE" };
            session.say(&format!("{tag} {answer}")).await?;
            return Ok(true);
        }
        let news = async {
            match &mut watching {
                Some(subscription) => subscription.next().await,
                // No mailbox is open, or the bus is gone: nothing will ever arrive this way.
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            read = reader.read_buf(buffer) => {
                if read? == 0 {
                    return Ok(false);
                }
            }
            change = news => {
                match change {
                    Some(_) => {
                        // Several changes usually come together; one look covers them all.
                        if let Some(subscription) = &mut watching {
                            while subscription.try_next().is_some() {}
                        }
                        session.announce().await?;
                    }
                    None => watching = None,
                }
            }
            () = tokio::time::sleep(IDLE_KEEPALIVE) => session.say("* OK Still here").await?,
        }
    }
}

/// Serves one IMAP connection until the client logs out, goes away or falls silent.
pub async fn serve<S>(stream: S, context: Arc<Context>) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut reader, writer) = tokio::io::split(stream);
    let mut session = Session::new(context, writer);
    session.greet().await?;

    let mut buffer: Vec<u8> = Vec::new();
    // Set while the client owes the response to AUTHENTICATE PLAIN.
    let mut awaiting_credentials: Option<String> = None;
    // How long the buffer has to become before the literal that was last agreed to is complete.
    let mut literal_agreed_until = 0usize;

    loop {
        // Work off whatever is complete in the buffer.
        loop {
            if awaiting_credentials.is_some() {
                let Some(line) = take_line(&mut buffer) else { break };
                let tag = awaiting_credentials.take().unwrap_or_default();
                if line == b"*" {
                    session.say(&format!("{tag} BAD Authentication cancelled")).await?;
                    continue;
                }
                match base64::engine::general_purpose::STANDARD.decode(&line) {
                    Ok(response) => session.authenticate_plain(&tag, &response).await?,
                    Err(_) => session.say(&format!("{tag} BAD The response is not base64")).await?,
                }
                continue;
            }
            if buffer.is_empty() {
                break;
            }
            match CommandCodec::default().decode(&buffer) {
                Ok((rest, command)) => {
                    let consumed = buffer.len() - rest.len();
                    let next = session.handle(command).await?;
                    buffer.drain(..consumed);
                    literal_agreed_until = 0;
                    match next {
                        Next::Continue => {}
                        Next::AuthData { tag } => awaiting_credentials = Some(tag),
                        Next::Idle { tag } => {
                            if !idle(&mut session, &mut reader, &mut buffer, &tag).await? {
                                return Ok(());
                            }
                        }
                        Next::Close => return Ok(()),
                    }
                }
                Err(CommandDecodeError::Incomplete) => break,
                Err(CommandDecodeError::LiteralFound { tag, length, mode }) => {
                    if buffer.len() < literal_agreed_until {
                        // Still receiving a literal that was agreed to.
                        break;
                    }
                    if length as usize > session.longest_literal() {
                        // The client may already be sending the data, and nothing after a refused
                        // literal can be trusted to be a command. End the conversation.
                        session.say(&format!("{} BAD The command is too long", tag.inner())).await?;
                        session.say("* BYE Closing the connection").await?;
                        return Ok(());
                    }
                    literal_agreed_until = buffer.len() + length as usize;
                    if mode == LiteralMode::Sync {
                        session.say("+ OK").await?;
                    }
                    break;
                }
                Err(CommandDecodeError::Failed) => {
                    let line = take_line(&mut buffer).unwrap_or_else(|| std::mem::take(&mut buffer));
                    literal_agreed_until = 0;
                    debug!("a command could not be parsed");
                    match tag_of(&line) {
                        Some(tag) => session.say(&format!("{tag} BAD The command could not be parsed")).await?,
                        None => session.say("* BAD The command could not be parsed").await?,
                    }
                }
            }
        }

        if buffer.len() > LONGEST_COMMAND + session.longest_literal() {
            session.say("* BYE The command is too long").await?;
            return Ok(());
        }
        let patience = if session.authenticated() { PATIENCE_AFTER_LOGIN } else { PATIENCE_BEFORE_LOGIN };
        match tokio::time::timeout(patience, reader.read_buf(&mut buffer)).await {
            Ok(Ok(0)) => return Ok(()),
            Ok(Ok(_)) => {}
            Ok(Err(error)) => return Err(error),
            Err(_) => {
                session.say("* BYE Closing an idle connection").await?;
                return Ok(());
            }
        }
    }
}
