// SPDX-FileCopyrightText: 2026 Sigmastone OÜ
// SPDX-License-Identifier: Apache-2.0

use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;

use super::fresh_address;
use crate::address::EmailAddress;
use crate::inbound::{InboundSource, Verdict, Verdicts};

/// How the suite makes mail arrive. Each backend has its own way: the in-memory source is pushed
/// to, a cloud source has a message placed where the mail service would place it.
#[async_trait]
pub trait InboundFixture: Send + Sync {
    /// Makes one message arrive at the source under test.
    async fn deliver(&self, recipients: Vec<EmailAddress>, verdicts: Verdicts, raw: Bytes);
}

/// Runs the [`InboundSource`] contract against `source`, which must have nothing waiting.
///
/// `patience` is how long the suite waits for a message it has just delivered.
pub async fn inbound_source(source: &dyn InboundSource, fixture: &dyn InboundFixture, patience: Duration) {
    let nothing = source.receive(10, Duration::from_millis(50)).await.expect("receive");
    assert!(nothing.is_empty(), "nothing was delivered, nothing is received");

    let recipients = vec![fresh_address(), fresh_address()];
    let verdicts = Verdicts { spam: Verdict::Fail, virus: Verdict::Pass, spf: Verdict::Pass, ..Verdicts::default() };
    let raw = Bytes::from_static(b"Subject: one\r\n\r\nfirst\r\n");
    fixture.deliver(recipients.clone(), verdicts, raw.clone()).await;

    let batch = source.receive(10, patience).await.expect("receive");
    assert_eq!(batch.len(), 1);
    let message = &batch[0];
    assert!(!message.id.is_empty());
    assert_eq!(message.recipients, recipients, "envelope recipients arrive in order");
    assert_eq!(message.verdicts, verdicts);
    assert_eq!(source.fetch(message).await.expect("fetch"), raw);
    assert_eq!(source.fetch(message).await.expect("fetch again"), raw, "fetching does not consume");

    source.acknowledge(message).await.expect("acknowledge");
    source.acknowledge(message).await.expect("acknowledging twice is not an error");
    let nothing = source.receive(10, Duration::from_millis(50)).await.expect("receive");
    assert!(nothing.is_empty(), "an acknowledged message does not come back");

    // `max` is honoured, and nothing is lost over several calls.
    for number in 0..3 {
        let raw = Bytes::from(format!("Subject: batch {number}\r\n\r\n"));
        fixture.deliver(vec![fresh_address()], Verdicts::default(), raw).await;
    }
    let mut seen = Vec::new();
    while seen.len() < 3 {
        let batch = source.receive(2, patience).await.expect("receive");
        assert!(!batch.is_empty(), "delivered messages arrive within the patience given");
        assert!(batch.len() <= 2, "never more than asked for");
        for message in batch {
            source.acknowledge(&message).await.expect("acknowledge");
            seen.push(message.id);
        }
    }
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), 3, "each message has its own id");
}
