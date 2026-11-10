//! Checking a user name and app password, the same way for every protocol.

use std::sync::OnceLock;
use std::time::SystemTime;

use tracing::warn;

use crate::address::EmailAddress;
use crate::error::{Error, Result};
use crate::id::{AccountId, CredentialId};
use crate::metadata::MetadataStore;
use crate::model::AccountState;
use crate::password;

/// A hash to spend time on when there is no account to check, so that the answer takes as long
/// for an unknown user as for a wrong password.
fn decoy_hash() -> &'static str {
    static HASH: OnceLock<String> = OnceLock::new();
    HASH.get_or_init(|| password::hash(&password::generate()))
}

/// The account `login` and `secret` belong to, if they are right and the account is active.
///
/// The login is the account's primary address; the secret is matched against every app password
/// of the account, and the one that matched is marked as used. Wrong credentials are `Ok(None)`;
/// an error means the store could not be asked.
pub async fn check(store: &dyn MetadataStore, login: &[u8], secret: &[u8]) -> Result<Option<AccountId>> {
    let login = std::str::from_utf8(login).ok().and_then(|text| EmailAddress::parse(text).ok());
    let account = match &login {
        Some(login) => store.find_account_by_login(login).await?,
        None => None,
    };
    let account = account.filter(|account| account.state == AccountState::Active);
    let mut hashes: Vec<(Option<CredentialId>, String)> = match &account {
        Some(account) => {
            let credentials = store.list_credentials(account.id).await?;
            credentials.into_iter().map(|credential| (Some(credential.id), credential.secret_hash)).collect()
        }
        None => Vec::new(),
    };
    if hashes.is_empty() {
        hashes.push((None, decoy_hash().to_owned()));
    }
    let secret = String::from_utf8_lossy(secret).into_owned();
    // Verifying is slow by design; keep it off the threads that serve the other connections.
    let verifying = tokio::task::spawn_blocking(move || {
        hashes.into_iter().find(|(_, hash)| password::verify(&secret, hash)).and_then(|(id, _)| id)
    });
    let matched = verifying.await.map_err(Error::unavailable)?;
    let (Some(account), Some(credential)) = (account, matched) else { return Ok(None) };
    if let Err(error) = store.touch_credential(account.id, credential, SystemTime::now()).await {
        warn!(%error, "could not record the use of a credential");
    }
    Ok(Some(account.id))
}

/// Splits the response of SASL PLAIN: authorization identity, user name and password, separated
/// by zero bytes. `None` when it does not have exactly these three parts.
pub fn split_plain(response: &[u8]) -> Option<(&[u8], &[u8], &[u8])> {
    let mut fields = response.split(|byte| *byte == 0);
    match (fields.next(), fields.next(), fields.next(), fields.next()) {
        (Some(authorize_as), Some(login), Some(secret), None) => Some((authorize_as, login, secret)),
        _ => None,
    }
}
