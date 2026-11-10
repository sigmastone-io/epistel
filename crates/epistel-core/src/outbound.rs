//! The outbound relay: handing a message to the mail service for delivery.

use async_trait::async_trait;
use bytes::Bytes;

use crate::address::EmailAddress;
use crate::error::BoxError;

/// A message ready to leave.
#[derive(Clone, Debug)]
pub struct OutboundMessage {
    /// The envelope sender.
    pub sender: EmailAddress,
    /// The envelope recipients, including those that were only in `Bcc`.
    pub recipients: Vec<EmailAddress>,
    /// The message as it goes on the wire, headers and body.
    pub raw: Bytes,
}

/// What the relay accepts in one call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RelayLimits {
    /// Largest raw message, in bytes.
    pub max_message_size: u64,
    /// Most envelope recipients in one call. The caller splits longer lists.
    pub max_recipients: usize,
}

/// Why a message was not accepted for delivery.
#[derive(Debug, thiserror::Error)]
pub enum RelayError {
    /// The message is larger than [`RelayLimits::max_message_size`].
    #[error("message too large")]
    TooLarge,
    /// More recipients than [`RelayLimits::max_recipients`], or none.
    #[error("recipient count out of range")]
    RecipientCount,
    /// The mail service refused the message and will refuse it again.
    #[error("rejected: {0}")]
    Rejected(String),
    /// The mail service could not take the message now. Trying again later may work.
    #[error("temporarily unavailable: {0}")]
    Transient(#[source] BoxError),
}

/// Sends mail through the mail service.
///
/// Accepting a message means the mail service has taken responsibility for it. Delivery to the
/// remote host, retries and bounces are the service's business.
#[async_trait]
pub trait OutboundRelay: Send + Sync + 'static {
    fn limits(&self) -> RelayLimits;

    /// Hands over one message. Either all recipients are accepted or none.
    async fn send(&self, message: OutboundMessage) -> Result<(), RelayError>;
}
