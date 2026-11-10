//! The IMAP server of Epistel.
//!
//! [`serve`] runs one connection over any byte stream. Transport security is the caller's
//! business: the server hands this crate a stream that is already inside TLS, and the tests hand
//! it an in-memory pipe.
//!
//! This is the read path: logging in, listing and opening mailboxes, fetching and searching.
//! Mailboxes open read-only, and commands that would change mail are refused.

pub mod address;
pub mod fetch;
pub mod mime;
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

pub use session::{CAPABILITIES, Context};
use session::{Next, Session};

/// The longest command accepted, literals included. Nothing on the read path comes near it.
const LONGEST_COMMAND: usize = 64 * 1024;
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
                        Next::Close => return Ok(()),
                    }
                }
                Err(CommandDecodeError::Incomplete) => break,
                Err(CommandDecodeError::LiteralFound { tag, length, mode }) => {
                    if buffer.len() < literal_agreed_until {
                        // Still receiving a literal that was agreed to.
                        break;
                    }
                    if length as usize > LONGEST_COMMAND {
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

        if buffer.len() > LONGEST_COMMAND {
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
