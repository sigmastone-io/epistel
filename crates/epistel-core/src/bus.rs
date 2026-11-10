//! The change bus: how a session learns that someone else touched its mailbox.

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::error::Result;
use crate::id::MailboxId;
use crate::model::Uid;

/// What happened to a mailbox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    /// A message was added.
    Appended(Uid),
    /// The flags of a message changed.
    FlagsChanged(Uid),
    /// Messages were removed.
    Expunged(Vec<Uid>),
}

/// A change to one mailbox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailboxChange {
    pub mailbox: MailboxId,
    pub kind: ChangeKind,
}

/// The receiving end of a subscription. Dropping it ends the subscription.
#[derive(Debug)]
pub struct Subscription {
    changes: mpsc::UnboundedReceiver<MailboxChange>,
}

impl Subscription {
    /// Builds a subscription from the channel a bus implementation feeds.
    pub fn new(changes: mpsc::UnboundedReceiver<MailboxChange>) -> Self {
        Self { changes }
    }

    /// The next change. `None` once the bus is gone.
    pub async fn next(&mut self) -> Option<MailboxChange> {
        self.changes.recv().await
    }

    /// A change that is waiting already, without waiting for one.
    pub fn try_next(&mut self) -> Option<MailboxChange> {
        self.changes.try_recv().ok()
    }
}

/// Carries mailbox changes from whoever made them to the sessions that have the mailbox open.
///
/// A hint, not a record: a session that receives a change reads the store to learn the details,
/// and a session that missed one catches up the next time it reads. Changes to one mailbox reach
/// each subscriber in the order they were published.
#[async_trait]
pub trait ChangeBus: Send + Sync + 'static {
    /// Tells every current subscriber of the mailbox.
    async fn publish(&self, change: MailboxChange) -> Result<()>;

    /// Starts receiving the changes to one mailbox that are published from now on.
    async fn subscribe(&self, mailbox: MailboxId) -> Result<Subscription>;
}
