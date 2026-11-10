//! Implementations of every backend trait that live in the process.
//!
//! They hold nothing once the process ends. They exist for tests, for trying the server out
//! without any cloud account, and as the second implementation that keeps the traits honest.

mod blob;
mod bus;
mod inbound;
mod metadata;
mod outbound;

pub use blob::MemoryBlobStore;
pub use bus::MemoryChangeBus;
pub use inbound::MemoryInboundSource;
pub use metadata::MemoryMetadataStore;
pub use outbound::MemoryOutboundRelay;

use std::sync::{Mutex, MutexGuard, PoisonError};

/// Locks a mutex whose holder may have panicked. The state behind these locks is only ever
/// changed in steps that cannot be left half done, so it is still good after a panic.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
