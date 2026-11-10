use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::Notify;
use tokio::time::{Instant, timeout_at};

use super::lock;
use crate::address::EmailAddress;
use crate::error::{Error, Result};
use crate::inbound::{InboundMessage, InboundSource, Verdicts};

/// An [`InboundSource`] fed by hand, through [`MemoryInboundSource::push`].
#[derive(Default)]
pub struct MemoryInboundSource {
    state: Mutex<State>,
    arrived: Notify,
}

#[derive(Default)]
struct State {
    next_number: u64,
    waiting: VecDeque<Entry>,
    /// Handed out and not acknowledged yet, by receipt.
    in_flight: HashMap<String, Entry>,
}

struct Entry {
    id: String,
    recipients: Vec<EmailAddress>,
    verdicts: Verdicts,
    received: SystemTime,
    raw: Bytes,
    attempts: u32,
}

impl MemoryInboundSource {
    pub fn new() -> Self {
        Self::default()
    }

    /// Makes a message available, as if the mail service had just accepted it. Returns its id.
    pub fn push(&self, recipients: Vec<EmailAddress>, verdicts: Verdicts, raw: Bytes) -> String {
        let mut state = lock(&self.state);
        state.next_number += 1;
        let id = format!("memory-{}", state.next_number);
        state.waiting.push_back(Entry {
            id: id.clone(),
            recipients,
            verdicts,
            received: SystemTime::now(),
            raw,
            attempts: 0,
        });
        drop(state);
        self.arrived.notify_one();
        id
    }

    /// Makes every message that was handed out but not acknowledged available again, the way a
    /// real source does after a while. Returns how many there were.
    pub fn redeliver_unacknowledged(&self) -> usize {
        let mut state = lock(&self.state);
        let entries: Vec<_> = state.in_flight.drain().map(|(_, entry)| entry).collect();
        let count = entries.len();
        state.waiting.extend(entries);
        drop(state);
        if count > 0 {
            self.arrived.notify_one();
        }
        count
    }

    fn take(&self, max: usize) -> Vec<InboundMessage> {
        let mut state = lock(&self.state);
        let mut batch = Vec::new();
        while batch.len() < max {
            let Some(mut entry) = state.waiting.pop_front() else {
                break;
            };
            entry.attempts += 1;
            let receipt = format!("{}#{}", entry.id, entry.attempts);
            batch.push(InboundMessage {
                id: entry.id.clone(),
                recipients: entry.recipients.clone(),
                verdicts: entry.verdicts,
                received: entry.received,
                receipt: receipt.clone(),
            });
            state.in_flight.insert(receipt, entry);
        }
        batch
    }
}

#[async_trait]
impl InboundSource for MemoryInboundSource {
    async fn receive(&self, max: usize, wait: Duration) -> Result<Vec<InboundMessage>> {
        let deadline = Instant::now() + wait;
        loop {
            let batch = self.take(max);
            if !batch.is_empty() || max == 0 {
                return Ok(batch);
            }
            if timeout_at(deadline, self.arrived.notified()).await.is_err() {
                return Ok(Vec::new());
            }
        }
    }

    async fn fetch(&self, message: &InboundMessage) -> Result<Bytes> {
        lock(&self.state).in_flight.get(&message.receipt).map(|entry| entry.raw.clone()).ok_or(Error::NotFound)
    }

    async fn acknowledge(&self, message: &InboundMessage) -> Result<()> {
        lock(&self.state).in_flight.remove(&message.receipt);
        Ok(())
    }
}
