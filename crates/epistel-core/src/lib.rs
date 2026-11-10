//! Domain types and backend traits of the Epistel mail server.
//!
//! Everything outside the process sits behind one of five traits:
//!
//! | Trait | Responsibility |
//! |---|---|
//! | [`MetadataStore`] | accounts, credentials, addresses, mailboxes, message records |
//! | [`BlobStore`] | immutable raw messages |
//! | [`InboundSource`] | received messages with envelope and verdicts |
//! | [`OutboundRelay`] | sending one raw message to envelope recipients |
//! | [`ChangeBus`] | telling open sessions that a mailbox changed |
//!
//! The traits speak in mail terms and name no vendor. [`memory`] holds an implementation of each
//! that lives in the process, and `conformance` (behind the feature of the same name) holds the
//! suites that every implementation has to pass.

pub mod address;
pub mod auth;
pub mod blob;
pub mod bus;
pub mod directory;
pub mod error;
pub mod header_address;
pub mod id;
pub mod inbound;
pub mod ingest;
pub mod memory;
pub mod metadata;
pub mod mime;
pub mod model;
pub mod outbound;
pub mod password;

#[cfg(feature = "conformance")]
pub mod conformance;

pub use address::{AddressPattern, EmailAddress, InvalidAddress};
pub use blob::BlobStore;
pub use bus::{ChangeBus, ChangeKind, MailboxChange, Subscription};
pub use error::{BoxError, Error, Result};
pub use id::{AccountId, BlobId, CredentialId, InvalidId, MailboxId};
pub use inbound::{InboundMessage, InboundSource, Verdict, Verdicts};
pub use metadata::MetadataStore;
pub use outbound::{OutboundMessage, OutboundRelay, RelayError, RelayLimits};
