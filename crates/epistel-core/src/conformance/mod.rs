//! What every backend has to get right, as code.
//!
//! One function per trait. Each takes an implementation, exercises the contract written in the
//! trait's documentation, and panics on the first thing that does not hold. A backend crate calls
//! them from its tests; a new backend is done when they pass.
//!
//! The suites create their own accounts under random names in `example.org` and never look at
//! data they did not create, so they can run against a store that is not empty.

mod blob;
mod bus;
mod inbound;
mod metadata;
mod outbound;

pub use blob::blob_store;
pub use bus::change_bus;
pub use inbound::{InboundFixture, inbound_source};
pub use metadata::metadata_store;
pub use outbound::outbound_relay;

use crate::address::EmailAddress;
use crate::error::Error;
use crate::id::AccountId;

/// An address nobody used before.
fn fresh_address() -> EmailAddress {
    EmailAddress::parse(&format!("user-{}@example.org", AccountId::generate())).expect("generated address is valid")
}

/// A domain nobody used before.
fn fresh_domain() -> String {
    format!("{}.example.org", AccountId::generate())
}

#[track_caller]
fn assert_not_found<T: std::fmt::Debug>(result: Result<T, Error>) {
    assert!(matches!(result, Err(Error::NotFound)), "expected NotFound, got {result:?}");
}

#[track_caller]
fn assert_already_exists<T: std::fmt::Debug>(result: Result<T, Error>) {
    assert!(matches!(result, Err(Error::AlreadyExists)), "expected AlreadyExists, got {result:?}");
}

#[track_caller]
fn assert_conflict<T: std::fmt::Debug>(result: Result<T, Error>) {
    assert!(matches!(result, Err(Error::Conflict(_))), "expected Conflict, got {result:?}");
}
