//! Ingest: takes received mail from the inbound source and files it into mailboxes.
//!
//! The source offers each message at least once, and anything here can fail half way. The rules
//! that keep mail from being lost or doubled:
//!
//! - A message is acknowledged only after every recipient is dealt with. Anything else leaves it
//!   with the source, which offers it again later and sets it aside if it keeps failing.
//! - Filing carries a key made of the message and the account, and the store refuses a key it
//!   has seen. A message offered twice is filed once.
//! - A recipient nobody owns is dropped, not bounced. The mail service accepted the message
//!   already; a bounce from here would go to whoever the sender claimed to be.

use std::collections::BTreeSet;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, info, warn};

use crate::blob::BlobStore;
use crate::bus::{ChangeBus, ChangeKind, MailboxChange};
use crate::directory;
use crate::error::{Error, Result};
use crate::id::{AccountId, BlobId};
use crate::inbound::{InboundMessage, InboundSource, Verdict};
use crate::metadata::MetadataStore;
use crate::model::{Flags, Mailbox, MailboxRole, NewMessage, Once};

/// How many messages are asked for at a time.
const BATCH: usize = 10;
/// How long one request to the source waits for mail.
const WAIT: Duration = Duration::from_secs(20);
/// How long a filed delivery is remembered. Sources keep a message for two weeks at most.
const REMEMBER_FOR: Duration = Duration::from_secs(15 * 24 * 60 * 60);
/// The longest pause after the source failed.
const LONGEST_PAUSE: Duration = Duration::from_secs(60);

/// What became of one received message.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Filed {
    /// Accounts that got the message now.
    pub delivered: usize,
    /// Accounts that had it from an earlier attempt.
    pub duplicates: usize,
    /// Recipients no account owns.
    pub unknown: usize,
}

/// Files received mail.
pub struct Ingest {
    metadata: Arc<dyn MetadataStore>,
    blobs: Arc<dyn BlobStore>,
    inbound: Arc<dyn InboundSource>,
    changes: Arc<dyn ChangeBus>,
}

/// The mailbox a message goes to, by what the mail service found.
fn destination(message: &InboundMessage) -> MailboxRole {
    if message.verdicts.virus == Verdict::Fail {
        MailboxRole::Quarantine
    } else if message.verdicts.spam == Verdict::Fail {
        MailboxRole::Junk
    } else {
        MailboxRole::Inbox
    }
}

impl Ingest {
    pub fn new(
        metadata: Arc<dyn MetadataStore>,
        blobs: Arc<dyn BlobStore>,
        inbound: Arc<dyn InboundSource>,
        changes: Arc<dyn ChangeBus>,
    ) -> Self {
        Self { metadata, blobs, inbound, changes }
    }

    /// Files mail until `shutdown` completes. A message being filed at that moment is finished
    /// first.
    pub async fn run(&self, shutdown: impl Future<Output = ()>) {
        tokio::pin!(shutdown);
        let mut failures = 0u32;
        loop {
            let received = tokio::select! {
                () = &mut shutdown => return,
                received = self.inbound.receive(BATCH, WAIT) => received,
            };
            match received {
                Ok(messages) => {
                    failures = 0;
                    for message in &messages {
                        match self.file(message).await {
                            Ok(filed) => info!(
                                mail = %message.id,
                                delivered = filed.delivered,
                                duplicates = filed.duplicates,
                                unknown = filed.unknown,
                                "filed"
                            ),
                            Err(error) => warn!(mail = %message.id, %error, "not filed, left for another attempt"),
                        }
                    }
                }
                Err(error) => {
                    failures += 1;
                    let pause = Duration::from_secs(1 << failures.min(6)).min(LONGEST_PAUSE);
                    warn!(%error, pause_seconds = pause.as_secs(), "inbound source failed");
                    tokio::select! {
                        () = &mut shutdown => return,
                        () = tokio::time::sleep(pause) => {}
                    }
                }
            }
        }
    }

    /// Files one message for all its recipients and acknowledges it.
    ///
    /// On an error the message is not acknowledged; what was filed stays filed and is recognized
    /// when the message is offered again.
    pub async fn file(&self, message: &InboundMessage) -> Result<Filed> {
        let mut filed = Filed::default();
        // Two addresses of one account are one delivery, not two copies.
        let mut accounts = BTreeSet::new();
        for recipient in &message.recipients {
            match directory::resolve_recipient(&*self.metadata, recipient).await? {
                Some(account) => {
                    accounts.insert(account);
                }
                None => {
                    filed.unknown += 1;
                    info!(mail = %message.id, %recipient, "no account owns the recipient, dropped");
                }
            }
        }
        if accounts.is_empty() {
            self.inbound.acknowledge(message).await?;
            return Ok(filed);
        }

        let raw = self.inbound.fetch(message).await?;
        let size = raw.len() as u64;
        let blob = self.blobs.put(raw).await?;
        let mut failure = None;
        for account in accounts {
            match self.deliver(account, blob, size, message).await {
                Ok(Delivery::Filed) => filed.delivered += 1,
                Ok(Delivery::Duplicate) => filed.duplicates += 1,
                Ok(Delivery::AccountGone) => filed.unknown += 1,
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            }
        }
        if filed.delivered == 0 {
            // Nothing points at the blob. Leaving it behind costs storage, not correctness.
            if let Err(error) = self.blobs.delete(blob).await {
                warn!(mail = %message.id, %error, "could not remove an unused blob");
            }
        }
        if let Some(error) = failure {
            return Err(error);
        }
        self.inbound.acknowledge(message).await?;
        Ok(filed)
    }

    async fn deliver(&self, account: AccountId, blob: BlobId, size: u64, message: &InboundMessage) -> Result<Delivery> {
        let mailbox = match self.mailbox_for(account, destination(message)).await {
            Ok(mailbox) => mailbox,
            // Deleted between resolving the address and now.
            Err(Error::NotFound) => return Ok(Delivery::AccountGone),
            Err(error) => return Err(error),
        };
        let new = NewMessage {
            blob,
            size,
            internal_date: message.received,
            flags: Flags::default(),
            once: Some(Once { key: format!("{}:{account}", message.id), remember_for: REMEMBER_FOR }),
        };
        let record = match self.metadata.append_message(mailbox.id, new).await {
            Ok(record) => record,
            Err(Error::AlreadyExists) => {
                debug!(mail = %message.id, %account, "filed by an earlier attempt");
                return Ok(Delivery::Duplicate);
            }
            Err(error) => return Err(error),
        };
        // A hint for open sessions. They find the message on their next read without it.
        let change = MailboxChange { mailbox: mailbox.id, kind: ChangeKind::Appended(record.uid) };
        if let Err(error) = self.changes.publish(change).await {
            warn!(mail = %message.id, %error, "could not announce a new message");
        }
        Ok(Delivery::Filed)
    }

    /// The account's mailbox for `role`. Falls back to the inbox when the user removed that
    /// mailbox, and puts the default mailboxes back when even the inbox is gone.
    async fn mailbox_for(&self, account: AccountId, role: MailboxRole) -> Result<Mailbox> {
        let store = &*self.metadata;
        for attempt in 0..2 {
            for wanted in [role, MailboxRole::Inbox] {
                if let Some(mailbox) = directory::mailbox_with_role(store, account, wanted).await? {
                    return Ok(mailbox);
                }
            }
            if attempt == 0 {
                directory::ensure_default_mailboxes(store, account).await?;
            }
        }
        Err(Error::NotFound)
    }
}

enum Delivery {
    Filed,
    Duplicate,
    AccountGone,
}
