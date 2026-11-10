//! Ingest against the in-memory backends.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use epistel_core::directory;
use epistel_core::ingest::{Filed, Ingest};
use epistel_core::memory::{MemoryBlobStore, MemoryChangeBus, MemoryInboundSource, MemoryMetadataStore};
use epistel_core::model::{MailboxRole, MessageRecord, NewAccount, UidRange};
use epistel_core::{
    AccountId, AddressPattern, BlobId, BlobStore, ChangeBus, ChangeKind, EmailAddress, Error, InboundSource,
    MetadataStore, Result, Verdict, Verdicts,
};

const RAW: &[u8] = b"From: someone@elsewhere.example\r\nSubject: hello\r\n\r\nbody\r\n";
const SOON: Duration = Duration::from_secs(5);

/// A blob store that can be told to refuse the next write.
#[derive(Default)]
struct FlakyBlobs {
    inner: MemoryBlobStore,
    fail_next_put: AtomicBool,
}

#[async_trait]
impl BlobStore for FlakyBlobs {
    async fn put(&self, content: Bytes) -> Result<BlobId> {
        if self.fail_next_put.swap(false, Ordering::SeqCst) {
            return Err(Error::unavailable("the blob store is having a moment"));
        }
        self.inner.put(content).await
    }

    async fn get(&self, id: BlobId) -> Result<Bytes> {
        self.inner.get(id).await
    }

    async fn delete(&self, id: BlobId) -> Result<()> {
        self.inner.delete(id).await
    }
}

struct World {
    metadata: Arc<MemoryMetadataStore>,
    blobs: Arc<FlakyBlobs>,
    inbound: Arc<MemoryInboundSource>,
    changes: Arc<MemoryChangeBus>,
    ingest: Ingest,
}

fn address(text: &str) -> EmailAddress {
    text.parse().unwrap()
}

impl World {
    fn new() -> Self {
        let metadata = Arc::new(MemoryMetadataStore::new());
        let blobs = Arc::new(FlakyBlobs::default());
        let inbound = Arc::new(MemoryInboundSource::new());
        let changes = Arc::new(MemoryChangeBus::new());
        let ingest = Ingest::new(metadata.clone(), blobs.clone(), inbound.clone(), changes.clone());
        Self { metadata, blobs, inbound, changes, ingest }
    }

    async fn account(&self, login: &str) -> AccountId {
        let new = NewAccount { primary_address: address(login), display_name: String::new() };
        directory::provision_account(&*self.metadata, new).await.unwrap().id
    }

    fn arrive(&self, recipients: &[&str], verdicts: Verdicts) {
        let recipients = recipients.iter().map(|text| address(text)).collect();
        self.inbound.push(recipients, verdicts, Bytes::from_static(RAW));
    }

    /// Takes the one waiting message from the source and files it.
    async fn file_next(&self) -> Result<Filed> {
        let message = self.inbound.receive(1, SOON).await.unwrap().pop().expect("a message is waiting");
        self.ingest.file(&message).await
    }

    async fn contents(&self, account: AccountId, role: MailboxRole) -> Vec<MessageRecord> {
        let mailbox = directory::mailbox_with_role(&*self.metadata, account, role).await.unwrap().unwrap();
        self.metadata.messages(mailbox.id, UidRange::ALL).await.unwrap()
    }

    async fn nothing_waiting(&self) -> bool {
        self.inbound.redeliver_unacknowledged() == 0
            && self.inbound.receive(1, Duration::from_millis(20)).await.unwrap().is_empty()
    }
}

#[tokio::test]
async fn clean_mail_lands_in_the_inbox_and_open_sessions_hear_of_it() {
    let world = World::new();
    let ada = world.account("ada@example.org").await;
    let inbox = directory::mailbox_with_role(&*world.metadata, ada, MailboxRole::Inbox).await.unwrap().unwrap();
    let mut session = world.changes.subscribe(inbox.id).await.unwrap();

    world.arrive(&["ada@example.org"], Verdicts::default());
    assert_eq!(world.file_next().await.unwrap(), Filed { delivered: 1, duplicates: 0, unknown: 0 });

    let filed = world.contents(ada, MailboxRole::Inbox).await;
    assert_eq!(filed.len(), 1);
    assert_eq!(filed[0].size, RAW.len() as u64);
    assert_eq!(world.blobs.get(filed[0].blob).await.unwrap(), RAW);
    assert!(filed[0].flags.system.is_empty(), "new mail is unseen");
    assert_eq!(session.try_next().unwrap().kind, ChangeKind::Appended(filed[0].uid));
    assert!(world.nothing_waiting().await, "a filed message is acknowledged");
}

#[tokio::test]
async fn spam_goes_to_junk_and_infected_mail_to_quarantine() {
    let world = World::new();
    let ada = world.account("ada@example.org").await;

    world.arrive(&["ada@example.org"], Verdicts { spam: Verdict::Fail, ..Verdicts::default() });
    world.file_next().await.unwrap();
    world.arrive(&["ada@example.org"], Verdicts { spam: Verdict::Fail, virus: Verdict::Fail, ..Verdicts::default() });
    world.file_next().await.unwrap();
    // Failed authentication alone moves nothing: the mail service's spam verdict already weighs it.
    world.arrive(&["ada@example.org"], Verdicts { spf: Verdict::Fail, dmarc: Verdict::Fail, ..Verdicts::default() });
    world.file_next().await.unwrap();

    assert_eq!(world.contents(ada, MailboxRole::Junk).await.len(), 1);
    assert_eq!(world.contents(ada, MailboxRole::Quarantine).await.len(), 1);
    assert_eq!(world.contents(ada, MailboxRole::Inbox).await.len(), 1);
}

#[tokio::test]
async fn several_recipients_share_one_blob_and_two_addresses_of_one_account_get_one_copy() {
    let world = World::new();
    let ada = world.account("ada@example.org").await;
    let bob = world.account("bob@example.org").await;
    world.metadata.put_address(AddressPattern::parse("sales@example.org").unwrap(), ada).await.unwrap();

    world.arrive(&["ada@example.org", "bob@example.org", "sales@example.org"], Verdicts::default());
    assert_eq!(world.file_next().await.unwrap(), Filed { delivered: 2, duplicates: 0, unknown: 0 });

    let for_ada = world.contents(ada, MailboxRole::Inbox).await;
    let for_bob = world.contents(bob, MailboxRole::Inbox).await;
    assert_eq!((for_ada.len(), for_bob.len()), (1, 1));
    assert_eq!(for_ada[0].blob, for_bob[0].blob);
    assert_eq!(world.blobs.inner.len(), 1);
}

#[tokio::test]
async fn unknown_recipients_are_dropped_and_a_catch_all_takes_the_rest() {
    let world = World::new();
    let ada = world.account("ada@example.org").await;

    world.arrive(&["nobody@example.org", "ada@example.org"], Verdicts::default());
    assert_eq!(world.file_next().await.unwrap(), Filed { delivered: 1, duplicates: 0, unknown: 1 });

    world.arrive(&["nobody@example.org"], Verdicts::default());
    assert_eq!(world.file_next().await.unwrap(), Filed { delivered: 0, duplicates: 0, unknown: 1 });
    assert!(world.nothing_waiting().await, "mail for nobody is acknowledged, not retried for ever");
    assert_eq!(world.blobs.inner.len(), 1, "mail for nobody is not stored");

    world.metadata.put_address(AddressPattern::parse("*@example.org").unwrap(), ada).await.unwrap();
    world.arrive(&["nobody@example.org"], Verdicts::default());
    assert_eq!(world.file_next().await.unwrap(), Filed { delivered: 1, duplicates: 0, unknown: 0 });
    assert_eq!(world.contents(ada, MailboxRole::Inbox).await.len(), 2);
}

#[tokio::test]
async fn a_message_offered_twice_is_filed_once() {
    let world = World::new();
    let ada = world.account("ada@example.org").await;
    let bob = world.account("bob@example.org").await;
    world.arrive(&["ada@example.org", "bob@example.org"], Verdicts::default());

    // Filed, but the acknowledgement never reaches the source.
    let first = world.inbound.receive(1, SOON).await.unwrap().pop().unwrap();
    world.ingest.file(&first).await.unwrap();
    // The source knows nothing of the first attempt's acknowledgement in this scenario.
    world.inbound.push(first.recipients.clone(), first.verdicts, Bytes::from_static(RAW));
    let mut again = world.inbound.receive(1, SOON).await.unwrap().pop().unwrap();
    again.id = first.id.clone();

    assert_eq!(world.ingest.file(&again).await.unwrap(), Filed { delivered: 0, duplicates: 2, unknown: 0 });
    assert_eq!(world.contents(ada, MailboxRole::Inbox).await.len(), 1);
    assert_eq!(world.contents(bob, MailboxRole::Inbox).await.len(), 1);
    assert_eq!(world.blobs.inner.len(), 1, "the second attempt's copy is removed again");
}

#[tokio::test]
async fn a_failure_leaves_the_message_with_the_source_and_the_next_attempt_files_it() {
    let world = World::new();
    let ada = world.account("ada@example.org").await;
    world.arrive(&["ada@example.org"], Verdicts::default());

    world.blobs.fail_next_put.store(true, Ordering::SeqCst);
    assert!(world.file_next().await.is_err());
    assert!(world.contents(ada, MailboxRole::Inbox).await.is_empty());

    assert_eq!(world.inbound.redeliver_unacknowledged(), 1, "an unfiled message is not acknowledged");
    assert_eq!(world.file_next().await.unwrap(), Filed { delivered: 1, duplicates: 0, unknown: 0 });
    assert_eq!(world.contents(ada, MailboxRole::Inbox).await.len(), 1);
    assert!(world.nothing_waiting().await);
}

#[tokio::test]
async fn mail_still_arrives_when_the_user_deleted_the_special_mailboxes() {
    let world = World::new();
    let ada = world.account("ada@example.org").await;
    let spam = Verdicts { spam: Verdict::Fail, ..Verdicts::default() };

    let junk = directory::mailbox_with_role(&*world.metadata, ada, MailboxRole::Junk).await.unwrap().unwrap();
    world.metadata.delete_mailbox(junk.id).await.unwrap();
    world.arrive(&["ada@example.org"], spam);
    world.file_next().await.unwrap();
    assert_eq!(
        world.contents(ada, MailboxRole::Inbox).await.len(),
        1,
        "without a junk mailbox, spam goes to the inbox"
    );

    for mailbox in world.metadata.list_mailboxes(ada).await.unwrap() {
        world.metadata.delete_mailbox(mailbox.id).await.unwrap();
    }
    world.arrive(&["ada@example.org"], spam);
    world.file_next().await.unwrap();
    assert_eq!(world.contents(ada, MailboxRole::Junk).await.len(), 1, "the default mailboxes come back");
}

#[tokio::test]
async fn the_loop_files_what_arrives_and_stops_when_told() {
    let world = World::new();
    let ada = world.account("ada@example.org").await;
    let World { metadata, ingest, inbound, .. } = world;
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let running = tokio::spawn(async move {
        ingest
            .run(async {
                let _ = stopped.await;
            })
            .await
    });

    for _ in 0..3 {
        inbound.push(vec![address("ada@example.org")], Verdicts::default(), Bytes::from_static(RAW));
    }
    let inbox = directory::mailbox_with_role(&*metadata, ada, MailboxRole::Inbox).await.unwrap().unwrap();
    let mut waited = Duration::ZERO;
    while metadata.messages(inbox.id, UidRange::ALL).await.unwrap().len() < 3 {
        assert!(waited < SOON, "the loop files mail without being asked");
        tokio::time::sleep(Duration::from_millis(10)).await;
        waited += Duration::from_millis(10);
    }

    stop.send(()).unwrap();
    tokio::time::timeout(SOON, running).await.expect("the loop stops when told").unwrap();
}
