use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::task::JoinSet;

use super::{assert_already_exists, assert_conflict, assert_not_found, fresh_address, fresh_domain};
use crate::address::AddressPattern;
use crate::id::{AccountId, BlobId, CredentialId, MailboxId};
use crate::metadata::MetadataStore;
use crate::model::{
    Account, AccountState, FlagChange, Flags, Mailbox, MailboxName, MailboxRole, ModSeq, NewAccount, NewCredential,
    NewMessage, Once, SystemFlags, Uid, UidRange,
};

/// Runs the whole [`MetadataStore`] contract against `store`.
pub async fn metadata_store(store: Arc<dyn MetadataStore>) {
    accounts(&*store).await;
    credentials(&*store).await;
    addresses(&*store).await;
    mailboxes(&*store).await;
    messages(&*store).await;
    blob_references(&*store).await;
    concurrent_appends(store.clone()).await;
    deliveries(&*store).await;
    account_deletion(&*store).await;
}

async fn new_account(store: &dyn MetadataStore) -> Account {
    let new = NewAccount { primary_address: fresh_address(), display_name: "Conformance".to_owned() };
    store.create_account(new).await.expect("create account")
}

async fn new_mailbox(store: &dyn MetadataStore, account: AccountId, name: &str) -> Mailbox {
    store.create_mailbox(account, name_of(name), None).await.expect("create mailbox")
}

fn name_of(name: &str) -> MailboxName {
    MailboxName::parse(name).expect("valid mailbox name")
}

fn message(blob: BlobId) -> NewMessage {
    NewMessage { blob, size: 1234, internal_date: SystemTime::now(), flags: Flags::default(), once: None }
}

fn uid(value: u32) -> Uid {
    Uid::new(value).expect("non-zero")
}

async fn accounts(store: &dyn MetadataStore) {
    let address = fresh_address();
    let new = NewAccount { primary_address: address.clone(), display_name: "Ada".to_owned() };
    let account = store.create_account(new.clone()).await.expect("create account");
    assert_eq!(account.primary_address, address);
    assert_eq!(account.display_name, "Ada");
    assert_eq!(account.state, AccountState::Active);

    assert_eq!(store.get_account(account.id).await.expect("get account"), account);
    assert_eq!(store.find_account_by_login(&address).await.expect("find account"), Some(account.clone()));
    assert_eq!(store.find_account_by_login(&fresh_address()).await.expect("find nobody"), None);
    assert_eq!(
        store.get_address(&AddressPattern::Exact(address)).await.expect("primary address is owned"),
        Some(account.id),
        "creating an account gives it its primary address"
    );

    assert_already_exists(store.create_account(new).await);
    assert_not_found(store.get_account(AccountId::generate()).await);

    store.set_account_state(account.id, AccountState::Suspended).await.expect("suspend");
    assert_eq!(store.get_account(account.id).await.expect("get account").state, AccountState::Suspended);
    assert_not_found(store.set_account_state(AccountId::generate(), AccountState::Active).await);

    let other = new_account(store).await;
    let listed = store.list_accounts().await.expect("list accounts");
    assert!(listed.iter().any(|a| a.id == account.id) && listed.iter().any(|a| a.id == other.id));
    assert!(listed.is_sorted_by(|a, b| a.primary_address <= b.primary_address), "accounts are ordered by address");
}

async fn credentials(store: &dyn MetadataStore) {
    let account = new_account(store).await;
    assert!(store.list_credentials(account.id).await.expect("list").is_empty());

    let laptop = NewCredential { label: "laptop".to_owned(), secret_hash: "$hash$one".to_owned() };
    let phone = NewCredential { label: "phone".to_owned(), secret_hash: "$hash$two".to_owned() };
    let laptop = store.add_credential(account.id, laptop).await.expect("add credential");
    let phone = store.add_credential(account.id, phone).await.expect("add credential");
    assert_eq!(laptop.secret_hash, "$hash$one");
    assert_eq!(laptop.last_used, None);
    assert_ne!(laptop.id, phone.id);

    let listed = store.list_credentials(account.id).await.expect("list");
    assert_eq!(listed.iter().map(|c| c.id).collect::<Vec<_>>(), [laptop.id, phone.id], "oldest first");

    // Whole seconds, so that a backend storing timestamps coarsely still returns the same value.
    let used = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    store.touch_credential(account.id, phone.id, used).await.expect("touch");
    let listed = store.list_credentials(account.id).await.expect("list");
    assert_eq!(listed[1].last_used, Some(used));
    assert_eq!(listed[0].last_used, None);

    store.remove_credential(account.id, laptop.id).await.expect("remove");
    let listed = store.list_credentials(account.id).await.expect("list");
    assert_eq!(listed.iter().map(|c| c.id).collect::<Vec<_>>(), [phone.id]);

    assert_not_found(store.remove_credential(account.id, laptop.id).await);
    assert_not_found(store.touch_credential(account.id, CredentialId::generate(), used).await);
    assert_not_found(store.list_credentials(AccountId::generate()).await);
    let orphan = NewCredential { label: "x".to_owned(), secret_hash: "x".to_owned() };
    assert_not_found(store.add_credential(AccountId::generate(), orphan).await);
}

async fn addresses(store: &dyn MetadataStore) {
    let owner = new_account(store).await;
    let other = new_account(store).await;
    let primary = AddressPattern::Exact(owner.primary_address.clone());
    let alias = AddressPattern::Exact(fresh_address());
    let catch_all = AddressPattern::Domain(fresh_domain());

    store.put_address(alias.clone(), owner.id).await.expect("put alias");
    store.put_address(alias.clone(), owner.id).await.expect("putting again for the same owner changes nothing");
    store.put_address(catch_all.clone(), owner.id).await.expect("put catch-all");
    assert_already_exists(store.put_address(alias.clone(), other.id).await);
    assert_not_found(store.put_address(AddressPattern::Exact(fresh_address()), AccountId::generate()).await);

    assert_eq!(store.get_address(&alias).await.expect("get"), Some(owner.id));
    assert_eq!(store.get_address(&catch_all).await.expect("get"), Some(owner.id));
    assert_eq!(store.get_address(&AddressPattern::Exact(fresh_address())).await.expect("get"), None);

    let listed: BTreeSet<_> = store.list_addresses(owner.id).await.expect("list").into_iter().collect();
    assert_eq!(listed, BTreeSet::from([primary.clone(), alias.clone(), catch_all.clone()]));

    store.remove_address(&alias).await.expect("remove alias");
    assert_eq!(store.get_address(&alias).await.expect("get"), None);
    assert_not_found(store.remove_address(&alias).await);
    assert_conflict(store.remove_address(&primary).await);

    store.put_address(alias.clone(), other.id).await.expect("a freed address can go to someone else");
    assert_eq!(store.get_address(&alias).await.expect("get"), Some(other.id));
}

async fn mailboxes(store: &dyn MetadataStore) {
    let account = new_account(store).await;
    let other = new_account(store).await;

    let inbox = store.create_mailbox(account.id, MailboxName::inbox(), Some(MailboxRole::Inbox)).await.expect("create");
    assert_eq!(inbox.account, account.id);
    assert_eq!(inbox.role, Some(MailboxRole::Inbox));
    assert!(inbox.subscribed);
    assert_eq!(inbox.next_uid, Uid::FIRST);
    assert_eq!(store.get_mailbox(inbox.id).await.expect("get"), inbox);

    assert_already_exists(store.create_mailbox(account.id, MailboxName::inbox(), None).await);
    assert_not_found(store.create_mailbox(AccountId::generate(), MailboxName::inbox(), None).await);
    store.create_mailbox(other.id, MailboxName::inbox(), None).await.expect("names are per account");

    let work = new_mailbox(store, account.id, "Work").await;
    let archive = new_mailbox(store, account.id, "Archive").await;
    assert_eq!(store.find_mailbox(account.id, &name_of("Work")).await.expect("find"), Some(work.clone()));
    assert_eq!(store.find_mailbox(account.id, &name_of("work")).await.expect("find"), None, "names keep their case");
    let names: Vec<_> =
        store.list_mailboxes(account.id).await.expect("list").into_iter().map(|m| m.name.to_string()).collect();
    assert_eq!(names, ["Archive", "INBOX", "Work"], "ordered by name");

    let renamed = store.rename_mailbox(work.id, name_of("Projects")).await.expect("rename");
    assert_eq!((renamed.id, renamed.uid_validity), (work.id, work.uid_validity), "renaming keeps identity");
    assert_eq!(renamed.name, name_of("Projects"));
    assert_eq!(store.find_mailbox(account.id, &name_of("Work")).await.expect("find"), None);
    assert_already_exists(store.rename_mailbox(archive.id, name_of("Projects")).await);
    assert_not_found(store.rename_mailbox(MailboxId::generate(), name_of("Nowhere")).await);

    store.set_subscribed(archive.id, false).await.expect("unsubscribe");
    assert!(!store.get_mailbox(archive.id).await.expect("get").subscribed);

    assert!(store.delete_mailbox(archive.id).await.expect("delete").is_empty());
    assert_not_found(store.get_mailbox(archive.id).await);
    assert_not_found(store.delete_mailbox(archive.id).await);
    let recreated = new_mailbox(store, account.id, "Archive").await;
    assert_ne!(recreated.uid_validity, archive.uid_validity, "a recreated mailbox gets a new UIDVALIDITY");
}

async fn messages(store: &dyn MetadataStore) {
    let account = new_account(store).await;
    let mailbox = new_mailbox(store, account.id, "Messages").await;
    assert!(store.messages(mailbox.id, UidRange::ALL).await.expect("empty mailbox").is_empty());

    let mut records = Vec::new();
    for _ in 0..5 {
        records.push(store.append_message(mailbox.id, message(BlobId::generate())).await.expect("append"));
    }
    assert_eq!(records.iter().map(|r| r.uid.get()).collect::<Vec<_>>(), [1, 2, 3, 4, 5]);
    assert!(records.is_sorted_by(|a, b| a.modseq < b.modseq), "each append moves the modification sequence on");
    assert!(records[0].modseq > mailbox.highest_modseq);

    let state = store.get_mailbox(mailbox.id).await.expect("get");
    assert_eq!(state.next_uid, uid(6));
    assert_eq!(state.highest_modseq, records[4].modseq);

    assert_eq!(store.messages(mailbox.id, UidRange::ALL).await.expect("all"), records);
    let middle = store.messages(mailbox.id, UidRange { first: uid(2), last: uid(4) }).await.expect("range");
    assert_eq!(middle, records[1..4]);
    assert_eq!(store.messages(mailbox.id, UidRange::single(uid(5))).await.expect("single"), records[4..]);
    assert!(store.messages(mailbox.id, UidRange { first: uid(4), last: uid(2) }).await.expect("backwards").is_empty());
    assert!(store.messages(mailbox.id, UidRange { first: uid(6), last: Uid::MAX }).await.expect("beyond").is_empty());
    assert_not_found(store.messages(MailboxId::generate(), UidRange::ALL).await);
    assert_not_found(store.append_message(MailboxId::generate(), message(BlobId::generate())).await);

    // Flags.
    let seen = FlagChange::Add(SystemFlags::SEEN.into());
    let updated = store.update_flags(mailbox.id, uid(2), seen.clone()).await.expect("set flag");
    assert!(updated.flags.system.contains(SystemFlags::SEEN));
    assert!(updated.modseq > records[4].modseq, "a flag change moves the modification sequence on");
    assert_eq!(store.get_mailbox(mailbox.id).await.expect("get").highest_modseq, updated.modseq);
    assert_eq!(
        store.messages(mailbox.id, UidRange::single(uid(2))).await.expect("read back"),
        std::slice::from_ref(&updated)
    );

    let unchanged = store.update_flags(mailbox.id, uid(2), seen).await.expect("set flag again");
    assert_eq!(unchanged, updated, "a change that changes nothing moves nothing");
    assert_eq!(store.get_mailbox(mailbox.id).await.expect("get").highest_modseq, updated.modseq);

    let replaced = FlagChange::Replace((SystemFlags::FLAGGED | SystemFlags::DRAFT).into());
    let replaced = store.update_flags(mailbox.id, uid(2), replaced).await.expect("replace flags");
    assert_eq!(replaced.flags, Flags::from(SystemFlags::FLAGGED | SystemFlags::DRAFT));
    let removed = FlagChange::Remove(SystemFlags::DRAFT.into());
    let removed = store.update_flags(mailbox.id, uid(2), removed).await.expect("remove flag");
    assert_eq!(removed.flags, Flags::from(SystemFlags::FLAGGED));
    assert_not_found(store.update_flags(mailbox.id, uid(99), FlagChange::Add(Flags::default())).await);

    // Expunge.
    let before: ModSeq = store.get_mailbox(mailbox.id).await.expect("get").highest_modseq;
    let expunged = store.expunge(mailbox.id, &[uid(5), uid(99), uid(3)]).await.expect("expunge");
    assert_eq!(expunged.uids, [uid(3), uid(5)], "removed UIDs ascending, missing ones skipped");
    let released: BTreeSet<_> = expunged.released_blobs.into_iter().collect();
    assert_eq!(released, BTreeSet::from([records[2].blob, records[4].blob]));
    let after = store.get_mailbox(mailbox.id).await.expect("get");
    assert!(after.highest_modseq > before, "an expunge moves the modification sequence on");
    let left: Vec<_> = store.messages(mailbox.id, UidRange::ALL).await.expect("all").iter().map(|r| r.uid).collect();
    assert_eq!(left, [uid(1), uid(2), uid(4)]);

    let nothing = store.expunge(mailbox.id, &[uid(3)]).await.expect("expunge again");
    assert!(nothing.uids.is_empty() && nothing.released_blobs.is_empty());
    assert_eq!(store.get_mailbox(mailbox.id).await.expect("get").highest_modseq, after.highest_modseq);
    assert_not_found(store.expunge(MailboxId::generate(), &[uid(1)]).await);

    let next = store.append_message(mailbox.id, message(BlobId::generate())).await.expect("append");
    assert_eq!(next.uid, uid(6), "an expunged UID is never given out again");
}

async fn blob_references(store: &dyn MetadataStore) {
    let account = new_account(store).await;
    let first = new_mailbox(store, account.id, "First").await;
    let second = new_mailbox(store, account.id, "Second").await;
    let shared = BlobId::generate();
    let own = BlobId::generate();

    store.append_message(first.id, message(shared)).await.expect("append");
    store.append_message(first.id, message(shared)).await.expect("append the same blob again");
    store.append_message(second.id, message(shared)).await.expect("append to another mailbox");
    store.append_message(second.id, message(own)).await.expect("append");

    let one_gone = store.expunge(first.id, &[uid(1)]).await.expect("expunge");
    assert!(one_gone.released_blobs.is_empty(), "a blob with references left is not released");

    let released: BTreeSet<_> = store.delete_mailbox(second.id).await.expect("delete mailbox").into_iter().collect();
    assert_eq!(released, BTreeSet::from([own]), "deleting a mailbox releases only blobs nobody else holds");

    let last_gone = store.expunge(first.id, &[uid(2)]).await.expect("expunge");
    assert_eq!(last_gone.released_blobs, [shared], "the last reference releases the blob");
}

async fn concurrent_appends(store: Arc<dyn MetadataStore>) {
    const WRITERS: u32 = 40;
    let account = new_account(&*store).await;
    let mailbox = new_mailbox(&*store, account.id, "Busy").await;

    let mut tasks = JoinSet::new();
    for _ in 0..WRITERS {
        let store = store.clone();
        tasks.spawn(async move { store.append_message(mailbox.id, message(BlobId::generate())).await });
    }
    let mut uids = BTreeSet::new();
    let mut modseqs = BTreeSet::new();
    while let Some(joined) = tasks.join_next().await {
        let record = joined.expect("append task").expect("append");
        uids.insert(record.uid.get());
        modseqs.insert(record.modseq);
    }
    assert_eq!(uids.len(), WRITERS as usize, "concurrent appends get distinct UIDs");
    assert_eq!(modseqs.len(), WRITERS as usize, "concurrent appends get distinct modification sequences");
    let next_uid = store.get_mailbox(mailbox.id).await.expect("get").next_uid;
    assert!(next_uid.get() > *uids.last().expect("appended"), "the next UID is past every UID given out");
}

async fn deliveries(store: &dyn MetadataStore) {
    let account = new_account(store).await;
    let inbox = new_mailbox(store, account.id, "Deliveries").await;
    let other = new_mailbox(store, account.id, "Elsewhere").await;
    let day = Duration::from_secs(24 * 60 * 60);
    let once =
        |key: &str, blob| NewMessage { once: Some(Once { key: key.to_owned(), remember_for: day }), ..message(blob) };
    let key = format!("delivery-{}", BlobId::generate());
    let blob = BlobId::generate();

    let first = store.append_message(inbox.id, once(&key, blob)).await.expect("the first delivery is filed");
    assert_already_exists(store.append_message(inbox.id, once(&key, BlobId::generate())).await);
    assert_already_exists(store.append_message(other.id, once(&key, BlobId::generate())).await);
    assert_eq!(
        store.messages(inbox.id, UidRange::ALL).await.expect("all"),
        std::slice::from_ref(&first),
        "filed exactly once"
    );
    assert!(store.messages(other.id, UidRange::ALL).await.expect("all").is_empty());

    let second_key = format!("delivery-{}", BlobId::generate());
    let second = store.append_message(inbox.id, once(&second_key, blob)).await.expect("keys are independent");
    assert!(second.uid > first.uid);

    // A refused delivery took no reference: two records point at the blob, so two expunges release it.
    let one_gone = store.expunge(inbox.id, &[first.uid]).await.expect("expunge");
    assert!(one_gone.released_blobs.is_empty());
    let both_gone = store.expunge(inbox.id, &[second.uid]).await.expect("expunge");
    assert_eq!(both_gone.released_blobs, [blob]);

    assert_not_found(store.append_message(MailboxId::generate(), once("never-recorded", blob)).await);
}

async fn account_deletion(store: &dyn MetadataStore) {
    let account = new_account(store).await;
    let alias = AddressPattern::Exact(fresh_address());
    store.put_address(alias.clone(), account.id).await.expect("put alias");
    let credential = NewCredential { label: "tablet".to_owned(), secret_hash: "$hash".to_owned() };
    store.add_credential(account.id, credential).await.expect("add credential");
    let mailbox = new_mailbox(store, account.id, "Leftover").await;

    assert_conflict(store.delete_account(account.id).await);
    store.delete_mailbox(mailbox.id).await.expect("delete mailbox");
    store.delete_account(account.id).await.expect("delete account");

    assert_not_found(store.get_account(account.id).await);
    assert_not_found(store.delete_account(account.id).await);
    assert_eq!(store.find_account_by_login(&account.primary_address).await.expect("find"), None);
    assert_eq!(store.get_address(&alias).await.expect("get"), None, "deleting an account frees its addresses");
    let again = NewAccount { primary_address: account.primary_address, display_name: "Again".to_owned() };
    store.create_account(again).await.expect("the primary address is free again");
}
