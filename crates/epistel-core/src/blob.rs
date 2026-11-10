//! The blob store: raw messages, written once and never changed.

use async_trait::async_trait;
use bytes::Bytes;

use crate::error::Result;
use crate::id::BlobId;

/// Stores raw messages.
///
/// A blob is immutable. Who may delete it is decided by the reference counts the metadata store
/// keeps; this trait only stores and returns bytes.
#[async_trait]
pub trait BlobStore: Send + Sync + 'static {
    /// Stores `content` under a new identifier.
    async fn put(&self, content: Bytes) -> Result<BlobId>;

    /// The whole blob. `NotFound` if there is none with that identifier.
    async fn get(&self, id: BlobId) -> Result<Bytes>;

    /// Removes a blob. Removing one that does not exist is not an error, so that a clean-up
    /// interrupted half way can simply run again.
    async fn delete(&self, id: BlobId) -> Result<()>;
}
