//! Rules about accounts and addresses that hold for every metadata store.

use crate::address::{AddressPattern, EmailAddress};
use crate::error::{Error, Result};
use crate::id::AccountId;
use crate::metadata::MetadataStore;
use crate::model::{Account, Mailbox, MailboxName, MailboxRole, NewAccount};

/// The mailboxes every account starts with.
pub const DEFAULT_MAILBOXES: [(&str, MailboxRole); 7] = [
    ("INBOX", MailboxRole::Inbox),
    ("Drafts", MailboxRole::Drafts),
    ("Sent", MailboxRole::Sent),
    ("Junk", MailboxRole::Junk),
    ("Trash", MailboxRole::Trash),
    ("Archive", MailboxRole::Archive),
    ("Quarantine", MailboxRole::Quarantine),
];

/// Creates an account and its default mailboxes.
pub async fn provision_account(store: &dyn MetadataStore, new: NewAccount) -> Result<Account> {
    let account = store.create_account(new).await?;
    ensure_default_mailboxes(store, account.id).await?;
    Ok(account)
}

/// Creates whichever default mailboxes the account lacks. Safe to run again after a failure.
pub async fn ensure_default_mailboxes(store: &dyn MetadataStore, account: AccountId) -> Result<()> {
    for (name, role) in DEFAULT_MAILBOXES {
        let name = MailboxName::parse(name).expect("default mailbox names are valid");
        match store.create_mailbox(account, name, Some(role)).await {
            Ok(_) | Err(Error::AlreadyExists) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// The account that receives mail for `address`: the owner of the address itself, otherwise the
/// owner of its domain's catch-all.
pub async fn resolve_recipient(store: &dyn MetadataStore, address: &EmailAddress) -> Result<Option<AccountId>> {
    if let Some(account) = store.get_address(&AddressPattern::Exact(address.clone())).await? {
        return Ok(Some(account));
    }
    store.get_address(&AddressPattern::catch_all_for(address)).await
}

/// The account's mailbox with the given role, if it has one.
pub async fn mailbox_with_role(
    store: &dyn MetadataStore,
    account: AccountId,
    role: MailboxRole,
) -> Result<Option<Mailbox>> {
    let mailboxes = store.list_mailboxes(account).await?;
    Ok(mailboxes.into_iter().find(|mailbox| mailbox.role == Some(role)))
}
