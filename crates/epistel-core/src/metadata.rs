// SPDX-FileCopyrightText: 2026 Sigmastone OÜ
// SPDX-License-Identifier: Apache-2.0

//! The metadata store: everything about mail except the raw messages themselves.

use std::time::SystemTime;

use async_trait::async_trait;

use crate::address::{AddressPattern, EmailAddress};
use crate::error::Result;
use crate::id::{AccountId, BlobId, CredentialId, MailboxId};
use crate::model::{
    Account, AccountState, Credential, Expunged, FlagChange, Mailbox, MailboxName, MailboxRole, MessageRecord,
    NewAccount, NewCredential, NewMessage, Uid, UidRange,
};

/// Accounts, credentials, addresses, mailboxes and message records.
///
/// The methods are stated in mail terms so that a key-value store, a document store and a SQL
/// database can each implement them in their natural way. Every method is atomic: it either takes
/// full effect or none, also when several callers and several processes use the store at once.
/// Where a method has to hold more than one thing together, its documentation says so.
///
/// Errors: [`Error::NotFound`](crate::Error::NotFound) when the account, mailbox or other thing
/// named by an argument does not exist, unless the method says otherwise.
#[async_trait]
pub trait MetadataStore: Send + Sync + 'static {
    // -- accounts ------------------------------------------------------------------------------

    /// Creates an active account that owns its primary address.
    ///
    /// Account and address appear together. `AlreadyExists` if the address is owned already.
    async fn create_account(&self, new: NewAccount) -> Result<Account>;

    async fn get_account(&self, id: AccountId) -> Result<Account>;

    /// The account whose primary address is `login`, if there is one.
    async fn find_account_by_login(&self, login: &EmailAddress) -> Result<Option<Account>>;

    /// Every account, ordered by primary address.
    async fn list_accounts(&self) -> Result<Vec<Account>>;

    async fn set_account_state(&self, id: AccountId, state: AccountState) -> Result<()>;

    /// Removes an account together with its credentials and addresses.
    ///
    /// `Conflict` while the account still has mailboxes; the caller deletes those first, because
    /// doing so releases blobs it has to clean up.
    async fn delete_account(&self, id: AccountId) -> Result<()>;

    // -- credentials ---------------------------------------------------------------------------

    async fn add_credential(&self, account: AccountId, new: NewCredential) -> Result<Credential>;

    /// The credentials of an account, oldest first.
    async fn list_credentials(&self, account: AccountId) -> Result<Vec<Credential>>;

    /// Records that a credential was used to log in.
    async fn touch_credential(&self, account: AccountId, credential: CredentialId, at: SystemTime) -> Result<()>;

    async fn remove_credential(&self, account: AccountId, credential: CredentialId) -> Result<()>;

    // -- addresses -----------------------------------------------------------------------------

    /// Gives `pattern` to `account`.
    ///
    /// Doing it again for the same account changes nothing. `AlreadyExists` if another account
    /// owns the pattern.
    async fn put_address(&self, pattern: AddressPattern, account: AccountId) -> Result<()>;

    /// The owner of exactly this pattern. Does not fall back from an address to its catch-all;
    /// see [`directory::resolve_recipient`](crate::directory::resolve_recipient) for that.
    async fn get_address(&self, pattern: &AddressPattern) -> Result<Option<AccountId>>;

    /// The patterns an account owns, in no particular order.
    async fn list_addresses(&self, account: AccountId) -> Result<Vec<AddressPattern>>;

    /// Takes a pattern away from its owner. `Conflict` for an account's primary address.
    async fn remove_address(&self, pattern: &AddressPattern) -> Result<()>;

    // -- mailboxes -----------------------------------------------------------------------------

    /// Creates an empty, subscribed mailbox.
    ///
    /// `AlreadyExists` if the account has a mailbox of that name. The new mailbox gets a
    /// `UIDVALIDITY` that no earlier mailbox of the same account and name ever had, so a client
    /// that cached the old one notices the replacement.
    async fn create_mailbox(&self, account: AccountId, name: MailboxName, role: Option<MailboxRole>)
    -> Result<Mailbox>;

    async fn get_mailbox(&self, id: MailboxId) -> Result<Mailbox>;

    async fn find_mailbox(&self, account: AccountId, name: &MailboxName) -> Result<Option<Mailbox>>;

    /// The mailboxes of an account, ordered by name.
    async fn list_mailboxes(&self, account: AccountId) -> Result<Vec<Mailbox>>;

    /// Renames a mailbox. Identifier, messages, UIDs and `UIDVALIDITY` stay.
    ///
    /// `AlreadyExists` if the account has a mailbox of the new name. Only this mailbox is
    /// renamed; moving its children along is the caller's job.
    async fn rename_mailbox(&self, id: MailboxId, new_name: MailboxName) -> Result<Mailbox>;

    async fn set_subscribed(&self, id: MailboxId, subscribed: bool) -> Result<()>;

    /// Removes a mailbox and all its message records.
    ///
    /// Returns the blobs no record points at any more; the caller deletes them from the blob
    /// store.
    async fn delete_mailbox(&self, id: MailboxId) -> Result<Vec<BlobId>>;

    // -- messages ------------------------------------------------------------------------------

    /// Adds a message to a mailbox.
    ///
    /// The message gets a UID larger than any the mailbox gave out before, and a modification
    /// sequence larger than any before; the mailbox's next UID and highest modification sequence
    /// move past them. Concurrent appends never share a UID, and a UID is never given out twice,
    /// not even after its message was expunged. Storing the record and giving the blob one more
    /// reference happen together or not at all. An append that fails may leave a UID unused.
    ///
    /// `Conflict` when the mailbox has run out of UIDs, and when the blob was reported released
    /// by an earlier [`expunge`](Self::expunge) or [`delete_mailbox`](Self::delete_mailbox): the
    /// caller was told to delete it, so nothing may point at it again. A backend that cannot
    /// tell may accept such an append; callers must not rely on either.
    ///
    /// With [`NewMessage::once`] set, the key is recorded together with the record, and an
    /// append whose key is still remembered is refused with `AlreadyExists` and changes nothing
    /// but, possibly, the mailbox's counters. Inbound sources offer a message at least once;
    /// this is what makes filing happen exactly once.
    async fn append_message(&self, mailbox: MailboxId, new: NewMessage) -> Result<MessageRecord>;

    /// The records whose UID lies in `range`, ascending.
    async fn messages(&self, mailbox: MailboxId, range: UidRange) -> Result<Vec<MessageRecord>>;

    /// Changes the flags of one message.
    ///
    /// When the change makes a difference, the message and the mailbox move to a new modification
    /// sequence; when it does not, nothing moves. Returns the record as it is afterwards.
    /// `NotFound` if the mailbox has no message with that UID.
    async fn update_flags(&self, mailbox: MailboxId, uid: Uid, change: FlagChange) -> Result<MessageRecord>;

    /// Removes messages for good.
    ///
    /// UIDs that do not exist are skipped. Each removed record gives up its reference to its
    /// blob; the result lists the blobs left without any. If anything was removed the mailbox
    /// moves to a new modification sequence.
    async fn expunge(&self, mailbox: MailboxId, uids: &[Uid]) -> Result<Expunged>;
}
