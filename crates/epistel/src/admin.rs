// SPDX-FileCopyrightText: 2026 Sigmastone OÜ
// SPDX-License-Identifier: Apache-2.0

//! The administration commands: accounts, app passwords and addresses.
//!
//! They act on the metadata store directly, with the same configuration as the server, so they
//! work whether or not a server is running.

use std::time::SystemTime;

use clap::Subcommand;
use epistel_core::model::{Account, AccountState, NewAccount, NewCredential};
use epistel_core::{AddressPattern, CredentialId, EmailAddress, Error, directory, password};

use crate::backends::Backends;

#[derive(Subcommand)]
pub enum AccountCommand {
    /// Creates an account with its default mailboxes. The address becomes the login name.
    Add {
        address: EmailAddress,
        /// The person's or function's name.
        #[arg(long, default_value = "")]
        name: String,
    },
    /// Lists every account.
    List,
    /// Refuses logins for an account. Mail is still delivered.
    Suspend { address: EmailAddress },
    /// Lets a suspended account log in again.
    Resume { address: EmailAddress },
    /// Deletes an account with all its mail. There is no way back.
    Remove { address: EmailAddress },
}

#[derive(Subcommand)]
pub enum PasswordCommand {
    /// Creates an app password and prints it once.
    New {
        address: EmailAddress,
        /// What the password is for, typically the device.
        #[arg(long, default_value = "")]
        label: String,
    },
    /// Lists the app passwords of an account, without the passwords.
    List { address: EmailAddress },
    /// Makes an app password stop working.
    Revoke { address: EmailAddress, id: CredentialId },
}

#[derive(Subcommand)]
pub enum AddressCommand {
    /// Gives an account another address, or a whole domain as `*@domain`.
    Add { pattern: AddressPattern, account: EmailAddress },
    /// Lists what an account receives mail for.
    List { account: EmailAddress },
    /// Takes an address away from its account.
    Remove { pattern: AddressPattern },
}

pub enum AdminCommand {
    Account(AccountCommand),
    Password(PasswordCommand),
    Address(AddressCommand),
}

#[derive(Debug, thiserror::Error)]
pub enum AdminError {
    #[error("no account has the login {0}")]
    NoSuchAccount(EmailAddress),
    #[error("{0} is in use already")]
    Taken(String),
    #[error("{0} does not exist")]
    Missing(String),
    #[error("{0}")]
    Refused(&'static str),
    #[error(transparent)]
    Backend(Error),
}

/// Turns a store error into a message about `what` the command was dealing with.
fn about(what: impl ToString) -> impl FnOnce(Error) -> AdminError {
    move |error| match error {
        Error::AlreadyExists => AdminError::Taken(what.to_string()),
        Error::NotFound => AdminError::Missing(what.to_string()),
        Error::Conflict(rule) => AdminError::Refused(rule),
        other => AdminError::Backend(other),
    }
}

async fn account_of(backends: &Backends, login: &EmailAddress) -> Result<Account, AdminError> {
    let found = backends.metadata.find_account_by_login(login).await.map_err(AdminError::Backend)?;
    found.ok_or_else(|| AdminError::NoSuchAccount(login.clone()))
}

fn age(time: SystemTime) -> String {
    let seconds = SystemTime::now().duration_since(time).unwrap_or_default().as_secs();
    match seconds {
        0..60 => "just now".to_owned(),
        60..3600 => format!("{} min ago", seconds / 60),
        3600..86_400 => format!("{} h ago", seconds / 3600),
        _ => format!("{} d ago", seconds / 86_400),
    }
}

/// Runs one command and returns the lines to print.
pub async fn run(backends: &Backends, command: AdminCommand) -> Result<Vec<String>, AdminError> {
    let store = &*backends.metadata;
    match command {
        AdminCommand::Account(AccountCommand::Add { address, name }) => {
            let new = NewAccount { primary_address: address.clone(), display_name: name };
            let account = directory::provision_account(store, new).await.map_err(about(&address))?;
            Ok(vec![
                format!("created account {} ({})", account.primary_address, account.id),
                format!("next: epistel password new {}", account.primary_address),
            ])
        }
        AdminCommand::Account(AccountCommand::List) => {
            let accounts = store.list_accounts().await.map_err(AdminError::Backend)?;
            if accounts.is_empty() {
                return Ok(vec!["no accounts".to_owned()]);
            }
            Ok(accounts
                .iter()
                .map(|account| {
                    let state = match account.state {
                        AccountState::Active => "active",
                        AccountState::Suspended => "suspended",
                    };
                    format!("{:<40} {:<10} {}", account.primary_address, state, account.display_name)
                })
                .collect())
        }
        AdminCommand::Account(AccountCommand::Suspend { address }) => {
            let account = account_of(backends, &address).await?;
            store.set_account_state(account.id, AccountState::Suspended).await.map_err(about(&address))?;
            Ok(vec![format!("suspended {address}")])
        }
        AdminCommand::Account(AccountCommand::Resume { address }) => {
            let account = account_of(backends, &address).await?;
            store.set_account_state(account.id, AccountState::Active).await.map_err(about(&address))?;
            Ok(vec![format!("resumed {address}")])
        }
        AdminCommand::Account(AccountCommand::Remove { address }) => {
            let account = account_of(backends, &address).await?;
            let mailboxes = store.list_mailboxes(account.id).await.map_err(about(&address))?;
            let mut blobs = 0;
            for mailbox in &mailboxes {
                for blob in store.delete_mailbox(mailbox.id).await.map_err(about(&mailbox.name))? {
                    backends.blobs.delete(blob).await.map_err(AdminError::Backend)?;
                    blobs += 1;
                }
            }
            store.delete_account(account.id).await.map_err(about(&address))?;
            Ok(vec![format!("removed {address}: {} mailboxes, {blobs} stored messages", mailboxes.len())])
        }

        AdminCommand::Password(PasswordCommand::New { address, label }) => {
            let account = account_of(backends, &address).await?;
            let secret = password::generate();
            // Hashing is slow on purpose; keep it off the threads that serve everything else.
            let hashing = tokio::task::spawn_blocking({
                let secret = secret.clone();
                move || password::hash(&secret)
            });
            let secret_hash = hashing.await.expect("hashing does not panic");
            let new = NewCredential { label, secret_hash };
            let credential = store.add_credential(account.id, new).await.map_err(about(&address))?;
            Ok(vec![
                format!("app password for {address} (id {})", credential.id),
                String::new(),
                format!("    {secret}"),
                String::new(),
                "It is shown this once and cannot be recovered. Enter it in the mail client as the".to_owned(),
                format!("password for both incoming and outgoing mail; the user name is {address}."),
            ])
        }
        AdminCommand::Password(PasswordCommand::List { address }) => {
            let account = account_of(backends, &address).await?;
            let credentials = store.list_credentials(account.id).await.map_err(about(&address))?;
            if credentials.is_empty() {
                return Ok(vec![format!("{address} has no app passwords")]);
            }
            Ok(credentials
                .iter()
                .map(|credential| {
                    let used =
                        credential.last_used.map_or_else(|| "never used".to_owned(), |at| format!("used {}", age(at)));
                    format!(
                        "{}  created {:<12} {:<16} {}",
                        credential.id,
                        age(credential.created),
                        used,
                        credential.label
                    )
                })
                .collect())
        }
        AdminCommand::Password(PasswordCommand::Revoke { address, id }) => {
            let account = account_of(backends, &address).await?;
            store.remove_credential(account.id, id).await.map_err(about(format!("app password {id}")))?;
            Ok(vec![format!("revoked app password {id} of {address}")])
        }

        AdminCommand::Address(AddressCommand::Add { pattern, account }) => {
            let owner = account_of(backends, &account).await?;
            store.put_address(pattern.clone(), owner.id).await.map_err(about(&pattern))?;
            Ok(vec![format!("{account} now receives mail for {pattern}")])
        }
        AdminCommand::Address(AddressCommand::List { account }) => {
            let owner = account_of(backends, &account).await?;
            let mut patterns = store.list_addresses(owner.id).await.map_err(about(&account))?;
            patterns.sort();
            Ok(patterns.iter().map(ToString::to_string).collect())
        }
        AdminCommand::Address(AddressCommand::Remove { pattern }) => {
            store.remove_address(&pattern).await.map_err(about(&pattern))?;
            Ok(vec![format!("removed {pattern}")])
        }
    }
}

#[cfg(test)]
mod tests {
    use epistel_core::model::{MailboxRole, NewMessage};

    use super::*;
    use crate::config::Config;

    const MEMORY: &str = r#"
        server = { hostname = "mail.example.org" }
        tls = { certificate = "cert.pem", private_key = "key.pem" }
        metadata = { backend = "memory" }
        blobs = { backend = "memory" }
        inbound = { backend = "memory" }
        outbound = { backend = "memory" }
    "#;

    async fn backends() -> Backends {
        Backends::build(&Config::parse(MEMORY).unwrap()).await
    }

    fn address(text: &str) -> EmailAddress {
        text.parse().unwrap()
    }

    async fn account(backends: &Backends, command: AccountCommand) -> Result<Vec<String>, AdminError> {
        run(backends, AdminCommand::Account(command)).await
    }

    async fn add(backends: &Backends, login: &str) {
        account(backends, AccountCommand::Add { address: address(login), name: "Ada".into() }).await.unwrap();
    }

    #[tokio::test]
    async fn an_account_can_be_created_listed_suspended_and_resumed() {
        let backends = backends().await;
        add(&backends, "ada@example.org").await;
        let again =
            account(&backends, AccountCommand::Add { address: address("ada@example.org"), name: String::new() });
        assert!(matches!(again.await, Err(AdminError::Taken(_))));

        account(&backends, AccountCommand::Suspend { address: address("ada@example.org") }).await.unwrap();
        let listed = account(&backends, AccountCommand::List).await.unwrap();
        assert!(listed[0].contains("ada@example.org") && listed[0].contains("suspended") && listed[0].contains("Ada"));
        account(&backends, AccountCommand::Resume { address: address("ada@example.org") }).await.unwrap();
        assert!(account(&backends, AccountCommand::List).await.unwrap()[0].contains("active"));

        let nobody = account(&backends, AccountCommand::Suspend { address: address("nobody@example.org") }).await;
        assert!(matches!(nobody, Err(AdminError::NoSuchAccount(_))));
    }

    #[tokio::test]
    async fn a_new_password_is_shown_once_and_only_its_hash_is_stored() {
        let backends = backends().await;
        add(&backends, "ada@example.org").await;
        let new = PasswordCommand::New { address: address("ada@example.org"), label: "laptop".into() };
        let printed = run(&backends, AdminCommand::Password(new)).await.unwrap();
        let secret = printed[2].trim().to_owned();

        let owner = backends.metadata.find_account_by_login(&address("ada@example.org")).await.unwrap().unwrap();
        let stored = backends.metadata.list_credentials(owner.id).await.unwrap();
        assert_eq!(stored.len(), 1);
        assert!(!stored[0].secret_hash.contains(&secret));
        assert!(password::verify(&secret, &stored[0].secret_hash));
        assert!(!password::verify("something else", &stored[0].secret_hash));

        let list = PasswordCommand::List { address: address("ada@example.org") };
        let listed = run(&backends, AdminCommand::Password(list)).await.unwrap();
        assert!(listed[0].contains("laptop") && listed[0].contains("never used") && !listed[0].contains(&secret));

        let revoke = PasswordCommand::Revoke { address: address("ada@example.org"), id: stored[0].id };
        run(&backends, AdminCommand::Password(revoke)).await.unwrap();
        assert!(backends.metadata.list_credentials(owner.id).await.unwrap().is_empty());
        let revoke = PasswordCommand::Revoke { address: address("ada@example.org"), id: stored[0].id };
        assert!(matches!(run(&backends, AdminCommand::Password(revoke)).await, Err(AdminError::Missing(_))));
    }

    #[tokio::test]
    async fn addresses_can_be_added_listed_and_removed_but_not_the_login() {
        let backends = backends().await;
        add(&backends, "ada@example.org").await;
        add(&backends, "bob@example.org").await;
        let pattern = |text: &str| text.parse::<AddressPattern>().unwrap();
        let command = |command| run(&backends, AdminCommand::Address(command));

        command(AddressCommand::Add { pattern: pattern("sales@example.org"), account: address("ada@example.org") })
            .await
            .unwrap();
        command(AddressCommand::Add { pattern: pattern("*@example.org"), account: address("ada@example.org") })
            .await
            .unwrap();
        let taken =
            command(AddressCommand::Add { pattern: pattern("sales@example.org"), account: address("bob@example.org") });
        assert!(matches!(taken.await, Err(AdminError::Taken(_))));

        let listed = command(AddressCommand::List { account: address("ada@example.org") }).await.unwrap();
        assert_eq!(listed, ["ada@example.org", "sales@example.org", "*@example.org"]);

        command(AddressCommand::Remove { pattern: pattern("sales@example.org") }).await.unwrap();
        let login = command(AddressCommand::Remove { pattern: pattern("ada@example.org") }).await;
        assert!(matches!(login, Err(AdminError::Refused(_))));
    }

    #[tokio::test]
    async fn removing_an_account_removes_its_mail_and_frees_its_address() {
        let backends = backends().await;
        add(&backends, "ada@example.org").await;
        let owner = backends.metadata.find_account_by_login(&address("ada@example.org")).await.unwrap().unwrap();
        let inbox =
            directory::mailbox_with_role(&*backends.metadata, owner.id, MailboxRole::Inbox).await.unwrap().unwrap();
        let blob = backends.blobs.put("Subject: hi\r\n\r\n".into()).await.unwrap();
        let message =
            NewMessage { blob, size: 15, internal_date: SystemTime::now(), flags: Default::default(), once: None };
        backends.metadata.append_message(inbox.id, message).await.unwrap();

        let printed = account(&backends, AccountCommand::Remove { address: address("ada@example.org") }).await.unwrap();
        assert!(printed[0].contains("7 mailboxes, 1 stored messages"), "{printed:?}");
        assert!(matches!(backends.blobs.get(blob).await, Err(Error::NotFound)));
        add(&backends, "ada@example.org").await;
    }
}
