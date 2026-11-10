use std::collections::{BTreeMap, HashMap};
use std::num::NonZeroU32;
use std::sync::Mutex;
use std::time::SystemTime;

use async_trait::async_trait;

use super::lock;
use crate::address::{AddressPattern, EmailAddress};
use crate::error::{Error, Result};
use crate::id::{AccountId, BlobId, CredentialId, MailboxId};
use crate::metadata::MetadataStore;
use crate::model::{
    Account, AccountState, Credential, Expunged, FlagChange, Mailbox, MailboxName, MailboxRole, MessageRecord, ModSeq,
    NewAccount, NewCredential, NewMessage, Uid, UidRange, UidValidity,
};

/// A [`MetadataStore`] kept in memory. One lock around everything makes every method atomic.
#[derive(Default)]
pub struct MemoryMetadataStore {
    state: Mutex<State>,
}

impl MemoryMetadataStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[derive(Default)]
struct State {
    accounts: HashMap<AccountId, AccountEntry>,
    addresses: BTreeMap<AddressPattern, AccountId>,
    mailboxes: HashMap<MailboxId, MailboxEntry>,
    blob_references: HashMap<BlobId, u64>,
    deliveries: HashMap<String, SystemTime>,
    last_uid_validity: u32,
}

struct AccountEntry {
    account: Account,
    credentials: Vec<Credential>,
}

struct MailboxEntry {
    mailbox: Mailbox,
    messages: BTreeMap<Uid, MessageRecord>,
}

impl State {
    fn account(&mut self, id: AccountId) -> Result<&mut AccountEntry> {
        self.accounts.get_mut(&id).ok_or(Error::NotFound)
    }

    fn mailbox(&mut self, id: MailboxId) -> Result<&mut MailboxEntry> {
        self.mailboxes.get_mut(&id).ok_or(Error::NotFound)
    }

    fn name_taken(&self, account: AccountId, name: &MailboxName) -> bool {
        self.mailboxes.values().any(|entry| entry.mailbox.account == account && entry.mailbox.name == *name)
    }

    /// Drops one reference to each blob and returns those left without any.
    fn release(&mut self, blobs: impl IntoIterator<Item = BlobId>) -> Vec<BlobId> {
        let mut released = Vec::new();
        for blob in blobs {
            let Some(count) = self.blob_references.get_mut(&blob) else {
                continue;
            };
            *count -= 1;
            if *count == 0 {
                self.blob_references.remove(&blob);
                released.push(blob);
            }
        }
        released
    }
}

#[async_trait]
impl MetadataStore for MemoryMetadataStore {
    async fn create_account(&self, new: NewAccount) -> Result<Account> {
        let mut state = lock(&self.state);
        let pattern = AddressPattern::Exact(new.primary_address.clone());
        if state.addresses.contains_key(&pattern) {
            return Err(Error::AlreadyExists);
        }
        let account = Account {
            id: AccountId::generate(),
            primary_address: new.primary_address,
            display_name: new.display_name,
            state: AccountState::Active,
            created: SystemTime::now(),
        };
        state.addresses.insert(pattern, account.id);
        state.accounts.insert(account.id, AccountEntry { account: account.clone(), credentials: Vec::new() });
        Ok(account)
    }

    async fn get_account(&self, id: AccountId) -> Result<Account> {
        Ok(lock(&self.state).account(id)?.account.clone())
    }

    async fn find_account_by_login(&self, login: &EmailAddress) -> Result<Option<Account>> {
        let state = lock(&self.state);
        Ok(state.accounts.values().find(|entry| entry.account.primary_address == *login).map(|e| e.account.clone()))
    }

    async fn list_accounts(&self) -> Result<Vec<Account>> {
        let state = lock(&self.state);
        let mut accounts: Vec<_> = state.accounts.values().map(|entry| entry.account.clone()).collect();
        accounts.sort_by(|a, b| a.primary_address.cmp(&b.primary_address));
        Ok(accounts)
    }

    async fn set_account_state(&self, id: AccountId, new_state: AccountState) -> Result<()> {
        lock(&self.state).account(id)?.account.state = new_state;
        Ok(())
    }

    async fn delete_account(&self, id: AccountId) -> Result<()> {
        let mut state = lock(&self.state);
        state.account(id)?;
        if state.mailboxes.values().any(|entry| entry.mailbox.account == id) {
            return Err(Error::Conflict("account still has mailboxes"));
        }
        state.addresses.retain(|_, owner| *owner != id);
        state.accounts.remove(&id);
        Ok(())
    }

    async fn add_credential(&self, account: AccountId, new: NewCredential) -> Result<Credential> {
        let credential = Credential {
            id: CredentialId::generate(),
            label: new.label,
            secret_hash: new.secret_hash,
            created: SystemTime::now(),
            last_used: None,
        };
        lock(&self.state).account(account)?.credentials.push(credential.clone());
        Ok(credential)
    }

    async fn list_credentials(&self, account: AccountId) -> Result<Vec<Credential>> {
        Ok(lock(&self.state).account(account)?.credentials.clone())
    }

    async fn touch_credential(&self, account: AccountId, credential: CredentialId, at: SystemTime) -> Result<()> {
        let mut state = lock(&self.state);
        let credentials = &mut state.account(account)?.credentials;
        let found = credentials.iter_mut().find(|c| c.id == credential).ok_or(Error::NotFound)?;
        found.last_used = Some(at);
        Ok(())
    }

    async fn remove_credential(&self, account: AccountId, credential: CredentialId) -> Result<()> {
        let mut state = lock(&self.state);
        let credentials = &mut state.account(account)?.credentials;
        let position = credentials.iter().position(|c| c.id == credential).ok_or(Error::NotFound)?;
        credentials.remove(position);
        Ok(())
    }

    async fn put_address(&self, pattern: AddressPattern, account: AccountId) -> Result<()> {
        let mut state = lock(&self.state);
        state.account(account)?;
        match state.addresses.get(&pattern) {
            Some(owner) if *owner != account => Err(Error::AlreadyExists),
            Some(_) => Ok(()),
            None => {
                state.addresses.insert(pattern, account);
                Ok(())
            }
        }
    }

    async fn get_address(&self, pattern: &AddressPattern) -> Result<Option<AccountId>> {
        Ok(lock(&self.state).addresses.get(pattern).copied())
    }

    async fn list_addresses(&self, account: AccountId) -> Result<Vec<AddressPattern>> {
        let mut state = lock(&self.state);
        state.account(account)?;
        Ok(state.addresses.iter().filter(|(_, owner)| **owner == account).map(|(p, _)| p.clone()).collect())
    }

    async fn remove_address(&self, pattern: &AddressPattern) -> Result<()> {
        let mut state = lock(&self.state);
        let owner = *state.addresses.get(pattern).ok_or(Error::NotFound)?;
        let primary = AddressPattern::Exact(state.account(owner)?.account.primary_address.clone());
        if *pattern == primary {
            return Err(Error::Conflict("the primary address cannot be removed"));
        }
        state.addresses.remove(pattern);
        Ok(())
    }

    async fn create_mailbox(
        &self,
        account: AccountId,
        name: MailboxName,
        role: Option<MailboxRole>,
    ) -> Result<Mailbox> {
        let mut state = lock(&self.state);
        state.account(account)?;
        if state.name_taken(account, &name) {
            return Err(Error::AlreadyExists);
        }
        // One counter for the whole store: larger than needed, and trivially never repeats for a name.
        state.last_uid_validity += 1;
        let uid_validity =
            NonZeroU32::new(state.last_uid_validity).ok_or(Error::Conflict("out of UIDVALIDITY values"))?;
        let mailbox = Mailbox {
            id: MailboxId::generate(),
            account,
            name,
            role,
            subscribed: true,
            uid_validity: UidValidity(uid_validity),
            next_uid: Uid::FIRST,
            highest_modseq: ModSeq::default(),
        };
        state.mailboxes.insert(mailbox.id, MailboxEntry { mailbox: mailbox.clone(), messages: BTreeMap::new() });
        Ok(mailbox)
    }

    async fn get_mailbox(&self, id: MailboxId) -> Result<Mailbox> {
        Ok(lock(&self.state).mailbox(id)?.mailbox.clone())
    }

    async fn find_mailbox(&self, account: AccountId, name: &MailboxName) -> Result<Option<Mailbox>> {
        let mut state = lock(&self.state);
        state.account(account)?;
        let found = state.mailboxes.values().find(|e| e.mailbox.account == account && e.mailbox.name == *name);
        Ok(found.map(|entry| entry.mailbox.clone()))
    }

    async fn list_mailboxes(&self, account: AccountId) -> Result<Vec<Mailbox>> {
        let mut state = lock(&self.state);
        state.account(account)?;
        let mut mailboxes: Vec<_> =
            state.mailboxes.values().filter(|e| e.mailbox.account == account).map(|e| e.mailbox.clone()).collect();
        mailboxes.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(mailboxes)
    }

    async fn rename_mailbox(&self, id: MailboxId, new_name: MailboxName) -> Result<Mailbox> {
        let mut state = lock(&self.state);
        let account = state.mailbox(id)?.mailbox.account;
        if state.name_taken(account, &new_name) {
            return Err(Error::AlreadyExists);
        }
        let entry = state.mailbox(id)?;
        entry.mailbox.name = new_name;
        Ok(entry.mailbox.clone())
    }

    async fn set_subscribed(&self, id: MailboxId, subscribed: bool) -> Result<()> {
        lock(&self.state).mailbox(id)?.mailbox.subscribed = subscribed;
        Ok(())
    }

    async fn delete_mailbox(&self, id: MailboxId) -> Result<Vec<BlobId>> {
        let mut state = lock(&self.state);
        let entry = state.mailboxes.remove(&id).ok_or(Error::NotFound)?;
        Ok(state.release(entry.messages.into_values().map(|record| record.blob)))
    }

    async fn append_message(&self, mailbox: MailboxId, new: NewMessage) -> Result<MessageRecord> {
        let mut state = lock(&self.state);
        state.mailbox(mailbox)?;
        if let Some(once) = &new.once {
            let now = SystemTime::now();
            state.deliveries.retain(|_, expires| *expires > now);
            if state.deliveries.contains_key(&once.key) {
                return Err(Error::AlreadyExists);
            }
            state.deliveries.insert(once.key.clone(), now + once.remember_for);
        }
        let entry = state.mailbox(mailbox)?;
        let uid = entry.mailbox.next_uid;
        // The last UID is never given out, so that `next_uid` always has a value to hold.
        let following = uid.next().ok_or(Error::Conflict("mailbox has run out of UIDs"))?;
        let modseq = entry.mailbox.highest_modseq.next();
        let record = MessageRecord {
            mailbox,
            uid,
            blob: new.blob,
            size: new.size,
            internal_date: new.internal_date,
            flags: new.flags,
            modseq,
        };
        entry.mailbox.next_uid = following;
        entry.mailbox.highest_modseq = modseq;
        entry.messages.insert(uid, record.clone());
        *state.blob_references.entry(new.blob).or_default() += 1;
        Ok(record)
    }

    async fn messages(&self, mailbox: MailboxId, range: UidRange) -> Result<Vec<MessageRecord>> {
        let mut state = lock(&self.state);
        let entry = state.mailbox(mailbox)?;
        if range.first > range.last {
            return Ok(Vec::new());
        }
        Ok(entry.messages.range(range.first..=range.last).map(|(_, record)| record.clone()).collect())
    }

    async fn update_flags(&self, mailbox: MailboxId, uid: Uid, change: FlagChange) -> Result<MessageRecord> {
        let mut state = lock(&self.state);
        let entry = state.mailbox(mailbox)?;
        let record = entry.messages.get_mut(&uid).ok_or(Error::NotFound)?;
        if record.flags.apply(&change) {
            let modseq = entry.mailbox.highest_modseq.next();
            record.modseq = modseq;
            entry.mailbox.highest_modseq = modseq;
        }
        Ok(record.clone())
    }

    async fn expunge(&self, mailbox: MailboxId, uids: &[Uid]) -> Result<Expunged> {
        let mut state = lock(&self.state);
        let entry = state.mailbox(mailbox)?;
        let mut removed: Vec<_> = uids.iter().filter_map(|uid| entry.messages.remove(uid)).collect();
        removed.sort_by_key(|record| record.uid);
        if !removed.is_empty() {
            entry.mailbox.highest_modseq = entry.mailbox.highest_modseq.next();
        }
        Ok(Expunged {
            uids: removed.iter().map(|record| record.uid).collect(),
            released_blobs: state.release(removed.iter().map(|record| record.blob)),
        })
    }
}
