// SPDX-FileCopyrightText: 2026 Sigmastone OÜ
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use tokio::sync::mpsc;

use super::lock;
use crate::bus::{ChangeBus, MailboxChange, Subscription};
use crate::error::Result;
use crate::id::MailboxId;

/// A [`ChangeBus`] for one process. This is the real thing for a single-instance deployment, not
/// only a test double.
#[derive(Default)]
pub struct MemoryChangeBus {
    subscribers: Mutex<HashMap<MailboxId, Vec<mpsc::UnboundedSender<MailboxChange>>>>,
}

impl MemoryChangeBus {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ChangeBus for MemoryChangeBus {
    async fn publish(&self, change: MailboxChange) -> Result<()> {
        let mut subscribers = lock(&self.subscribers);
        if let Some(senders) = subscribers.get_mut(&change.mailbox) {
            // Sending fails only when the subscription was dropped; that is how one leaves.
            senders.retain(|sender| sender.send(change.clone()).is_ok());
            if senders.is_empty() {
                subscribers.remove(&change.mailbox);
            }
        }
        Ok(())
    }

    async fn subscribe(&self, mailbox: MailboxId) -> Result<Subscription> {
        let (sender, receiver) = mpsc::unbounded_channel();
        lock(&self.subscribers).entry(mailbox).or_default().push(sender);
        Ok(Subscription::new(receiver))
    }
}
