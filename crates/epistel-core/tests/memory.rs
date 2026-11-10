//! The in-memory backends against the conformance suites, and the rules in `directory` against
//! the in-memory store.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use epistel_core::conformance::{self, InboundFixture};
use epistel_core::directory::{self, DEFAULT_MAILBOXES};
use epistel_core::memory::{
    MemoryBlobStore, MemoryChangeBus, MemoryInboundSource, MemoryMetadataStore, MemoryOutboundRelay,
};
use epistel_core::model::{MailboxRole, NewAccount};
use epistel_core::{
    AddressPattern, EmailAddress, InboundSource, MetadataStore, OutboundMessage, OutboundRelay, RelayError, Verdicts,
};

const PATIENCE: Duration = Duration::from_secs(5);

fn address(text: &str) -> EmailAddress {
    EmailAddress::parse(text).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn metadata_store_conforms() {
    conformance::metadata_store(Arc::new(MemoryMetadataStore::new())).await;
}

#[tokio::test]
async fn blob_store_conforms() {
    let store = MemoryBlobStore::new();
    conformance::blob_store(&store).await;
    assert!(store.is_empty(), "the suite cleans up after itself");
}

#[async_trait]
impl InboundFixture for MemorySource {
    async fn deliver(&self, recipients: Vec<EmailAddress>, verdicts: Verdicts, raw: Bytes) {
        self.0.push(recipients, verdicts, raw);
    }
}

struct MemorySource(Arc<MemoryInboundSource>);

#[tokio::test]
async fn inbound_source_conforms() {
    let source = Arc::new(MemoryInboundSource::new());
    conformance::inbound_source(&*source, &MemorySource(source.clone()), PATIENCE).await;
}

#[tokio::test]
async fn outbound_relay_conforms() {
    let relay = MemoryOutboundRelay::default();
    conformance::outbound_relay(&relay, address("sender@example.org"), address("recipient@example.org")).await;
    assert_eq!(relay.sent().len(), 1, "only the message within the limits was accepted");
}

#[tokio::test]
async fn change_bus_conforms() {
    conformance::change_bus(&MemoryChangeBus::new(), PATIENCE).await;
}

#[tokio::test]
async fn unacknowledged_mail_comes_back_with_the_same_id() {
    let source = MemoryInboundSource::new();
    let id = source.push(vec![address("a@example.org")], Verdicts::default(), Bytes::from_static(b"raw"));

    let first = source.receive(1, PATIENCE).await.unwrap().remove(0);
    assert_eq!(first.id, id);
    assert_eq!(source.redeliver_unacknowledged(), 1);

    let second = source.receive(1, PATIENCE).await.unwrap().remove(0);
    assert_eq!(second.id, id, "the id is what de-duplication keys on");
    assert_ne!(second.receipt, first.receipt);
    assert!(source.fetch(&first).await.is_err(), "the earlier attempt's handle is void");
    assert_eq!(source.fetch(&second).await.unwrap(), Bytes::from_static(b"raw"));
}

#[tokio::test]
async fn receive_wakes_when_mail_arrives() {
    let source = Arc::new(MemoryInboundSource::new());
    let waiting = tokio::spawn({
        let source = source.clone();
        async move { source.receive(1, PATIENCE).await.unwrap() }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    source.push(vec![address("a@example.org")], Verdicts::default(), Bytes::new());
    assert_eq!(waiting.await.unwrap().len(), 1);
}

#[tokio::test]
async fn relay_failures_can_be_staged() {
    let relay = MemoryOutboundRelay::default();
    let message = OutboundMessage {
        sender: address("sender@example.org"),
        recipients: vec![address("recipient@example.org")],
        raw: Bytes::from_static(b"Subject: x\r\n\r\n"),
    };
    relay.fail_next(RelayError::Rejected("address is on the suppression list".to_owned()));
    assert!(matches!(relay.send(message.clone()).await, Err(RelayError::Rejected(_))));
    relay.send(message).await.unwrap();
    assert_eq!(relay.sent().len(), 1);
}

#[tokio::test]
async fn a_new_account_gets_the_default_mailboxes() {
    let store = MemoryMetadataStore::new();
    let new = NewAccount { primary_address: address("ada@example.org"), display_name: "Ada".to_owned() };
    let account = directory::provision_account(&store, new).await.unwrap();

    let mailboxes = store.list_mailboxes(account.id).await.unwrap();
    assert_eq!(mailboxes.len(), DEFAULT_MAILBOXES.len());
    for (name, role) in DEFAULT_MAILBOXES {
        let mailbox = directory::mailbox_with_role(&store, account.id, role).await.unwrap().unwrap();
        assert_eq!(mailbox.name.as_str(), name);
    }

    let junk = directory::mailbox_with_role(&store, account.id, MailboxRole::Junk).await.unwrap().unwrap();
    store.delete_mailbox(junk.id).await.unwrap();
    directory::ensure_default_mailboxes(&store, account.id).await.unwrap();
    assert_eq!(store.list_mailboxes(account.id).await.unwrap().len(), DEFAULT_MAILBOXES.len(), "missing ones return");
}

#[tokio::test]
async fn recipients_resolve_to_the_exact_owner_before_the_catch_all() {
    let store = MemoryMetadataStore::new();
    let account = |text: &str| NewAccount { primary_address: address(text), display_name: String::new() };
    let ada = store.create_account(account("ada@example.org")).await.unwrap();
    let postmaster = store.create_account(account("postmaster@example.org")).await.unwrap();
    store.put_address(AddressPattern::parse("*@example.org").unwrap(), postmaster.id).await.unwrap();
    store.put_address(AddressPattern::parse("sales@example.org").unwrap(), ada.id).await.unwrap();

    let resolve = |text: &'static str| {
        let store = &store;
        async move { directory::resolve_recipient(store, &address(text)).await.unwrap() }
    };
    assert_eq!(resolve("ada@example.org").await, Some(ada.id));
    assert_eq!(resolve("Sales@Example.org").await, Some(ada.id));
    assert_eq!(resolve("anything@example.org").await, Some(postmaster.id));
    assert_eq!(resolve("ada@elsewhere.example").await, None);
}
