// SPDX-FileCopyrightText: 2026 Sigmastone OÜ
// SPDX-License-Identifier: Apache-2.0

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

/// Whether mail for `address` goes to `account`: the account owns the address itself, or the
/// catch-all of its domain while nobody else owns the address. This is also what decides whether
/// the account may send as that address.
pub async fn owns(store: &dyn MetadataStore, account: AccountId, address: &EmailAddress) -> Result<bool> {
    Ok(resolve_recipient(store, address).await? == Some(account))
}

/// A mailbox the deployment says has to exist: an address that mail is received for, and
/// further addresses that deliver into the same mailbox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Declared {
    /// The address. It is also the login name.
    pub address: EmailAddress,
    pub name: String,
    pub aliases: Vec<AddressPattern>,
}

/// What [`declare`] found and did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeclareOutcome {
    /// The mailbox did not exist and was made.
    pub created: bool,
    /// Aliases that belong to another account and were left there.
    pub taken: Vec<AddressPattern>,
}

/// Makes sure a declared mailbox exists, with its default folders and its aliases.
///
/// Safe to run on every start: what exists is left alone, and nothing is ever removed. A
/// mailbox that is dropped from the declaration stays, with its mail, until someone removes it
/// by hand. `Conflict` when the address already delivers into another account's mailbox.
pub async fn declare(store: &dyn MetadataStore, declared: &Declared) -> Result<DeclareOutcome> {
    let mut outcome = DeclareOutcome::default();
    let account = match store.find_account_by_login(&declared.address).await? {
        Some(account) => account,
        None => {
            let new = NewAccount { primary_address: declared.address.clone(), display_name: declared.name.clone() };
            match store.create_account(new).await {
                Ok(account) => {
                    outcome.created = true;
                    account
                }
                // Made by another instance starting at the same moment, or the address is an
                // alias of someone else's mailbox.
                Err(Error::AlreadyExists) => match store.find_account_by_login(&declared.address).await? {
                    Some(account) => account,
                    None => return Err(Error::Conflict("the address belongs to another mailbox")),
                },
                Err(error) => return Err(error),
            }
        }
    };
    ensure_default_mailboxes(store, account.id).await?;
    for alias in &declared.aliases {
        match store.put_address(alias.clone(), account.id).await {
            Ok(()) => {}
            Err(Error::AlreadyExists) => outcome.taken.push(alias.clone()),
            Err(error) => return Err(error),
        }
    }
    Ok(outcome)
}
