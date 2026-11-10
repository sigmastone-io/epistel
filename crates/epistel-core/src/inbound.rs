//! The inbound source: mail the mail service accepted for our domains.

use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use bytes::Bytes;

use crate::address::EmailAddress;
use crate::error::Result;

/// The outcome of one check the mail service ran on a received message.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Fail,
    /// The check was not run or gave no clear answer.
    #[default]
    Unknown,
}

/// All checks the mail service ran on a received message.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Verdicts {
    pub spam: Verdict,
    pub virus: Verdict,
    pub spf: Verdict,
    pub dkim: Verdict,
    pub dmarc: Verdict,
}

/// A received message waiting to be filed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboundMessage {
    /// Identifies the message at the mail service. The same on every redelivery, which makes it
    /// the key for de-duplication.
    pub id: String,
    /// The envelope recipients. They are not in the message itself.
    pub recipients: Vec<EmailAddress>,
    pub verdicts: Verdicts,
    pub received: SystemTime,
    /// The source's handle for this delivery attempt. Opaque to everyone else.
    pub receipt: String,
}

/// Hands out received messages, at least once each.
///
/// A message that was received but not acknowledged is handed out again later. The source decides
/// when; a message that keeps coming back is the source's to set aside.
#[async_trait]
pub trait InboundSource: Send + Sync + 'static {
    /// Up to `max` messages. Waits up to `wait` for the first one and returns an empty list when
    /// none arrived.
    async fn receive(&self, max: usize, wait: Duration) -> Result<Vec<InboundMessage>>;

    /// The raw message.
    async fn fetch(&self, message: &InboundMessage) -> Result<Bytes>;

    /// Says the message is filed. The source forgets it and may remove its copy. Acknowledging
    /// twice is not an error.
    async fn acknowledge(&self, message: &InboundMessage) -> Result<()>;
}
