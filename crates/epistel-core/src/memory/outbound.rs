// SPDX-FileCopyrightText: 2026 Sigmastone OÜ
// SPDX-License-Identifier: Apache-2.0

use std::sync::Mutex;

use async_trait::async_trait;

use super::lock;
use crate::outbound::{OutboundMessage, OutboundRelay, RelayError, RelayLimits};

/// An [`OutboundRelay`] that keeps what it is given instead of sending it anywhere.
pub struct MemoryOutboundRelay {
    limits: RelayLimits,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    sent: Vec<OutboundMessage>,
    next_failure: Option<RelayError>,
}

impl MemoryOutboundRelay {
    pub fn new(limits: RelayLimits) -> Self {
        Self { limits, state: Mutex::default() }
    }

    /// Everything accepted so far, oldest first.
    pub fn sent(&self) -> Vec<OutboundMessage> {
        lock(&self.state).sent.clone()
    }

    /// Makes the next send fail with `error`, to show callers how a refusal looks.
    pub fn fail_next(&self, error: RelayError) {
        lock(&self.state).next_failure = Some(error);
    }
}

impl Default for MemoryOutboundRelay {
    /// The limits of a typical cloud mail service: 40 MiB, 50 recipients.
    fn default() -> Self {
        Self::new(RelayLimits { max_message_size: 40 * 1024 * 1024, max_recipients: 50 })
    }
}

#[async_trait]
impl OutboundRelay for MemoryOutboundRelay {
    fn limits(&self) -> RelayLimits {
        self.limits
    }

    async fn send(&self, message: OutboundMessage) -> Result<(), RelayError> {
        if message.raw.len() as u64 > self.limits.max_message_size {
            return Err(RelayError::TooLarge);
        }
        if message.recipients.is_empty() || message.recipients.len() > self.limits.max_recipients {
            return Err(RelayError::RecipientCount);
        }
        let mut state = lock(&self.state);
        if let Some(error) = state.next_failure.take() {
            return Err(error);
        }
        state.sent.push(message);
        Ok(())
    }
}
