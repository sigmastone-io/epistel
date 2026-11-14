// SPDX-FileCopyrightText: 2026 Sigmastone OÜ
// SPDX-License-Identifier: Apache-2.0

use std::time::Duration;

use tokio::time::timeout;

use crate::bus::{ChangeBus, ChangeKind, MailboxChange, Subscription};
use crate::id::MailboxId;
use crate::model::Uid;

async fn expect_next(subscription: &mut Subscription, patience: Duration) -> MailboxChange {
    timeout(patience, subscription.next()).await.expect("a published change arrives in time").expect("bus is alive")
}

async fn expect_silence(subscription: &mut Subscription) {
    let waited = timeout(Duration::from_millis(100), subscription.next()).await;
    assert!(waited.is_err(), "nothing was published for this subscriber, got {waited:?}");
}

/// Runs the [`ChangeBus`] contract against `bus`.
///
/// `patience` is how long a published change may take to reach a subscriber.
pub async fn change_bus(bus: &dyn ChangeBus, patience: Duration) {
    let watched = MailboxId::generate();
    let elsewhere = MailboxId::generate();
    let uid = |value| Uid::new(value).expect("non-zero");
    let change = |mailbox, kind| MailboxChange { mailbox, kind };

    bus.publish(change(watched, ChangeKind::Appended(uid(1)))).await.expect("publishing to nobody is fine");

    let mut first = bus.subscribe(watched).await.expect("subscribe");
    let mut second = bus.subscribe(watched).await.expect("subscribe");
    let mut other = bus.subscribe(elsewhere).await.expect("subscribe");
    expect_silence(&mut first).await;

    let sequence = [
        change(watched, ChangeKind::Appended(uid(2))),
        change(watched, ChangeKind::FlagsChanged(uid(2))),
        change(watched, ChangeKind::Expunged(vec![uid(1), uid(2)])),
    ];
    for item in &sequence {
        bus.publish(item.clone()).await.expect("publish");
    }
    for subscription in [&mut first, &mut second] {
        for item in &sequence {
            assert_eq!(expect_next(subscription, patience).await, *item, "every subscriber, in order");
        }
    }
    expect_silence(&mut other).await;

    drop(first);
    let late = change(watched, ChangeKind::Appended(uid(3)));
    bus.publish(late.clone()).await.expect("a dropped subscriber does not break publishing");
    assert_eq!(expect_next(&mut second, patience).await, late);

    let elsewhere_change = change(elsewhere, ChangeKind::Appended(uid(1)));
    bus.publish(elsewhere_change.clone()).await.expect("publish");
    assert_eq!(expect_next(&mut other, patience).await, elsewhere_change);
    expect_silence(&mut second).await;
}
