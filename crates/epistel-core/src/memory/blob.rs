use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use bytes::Bytes;

use super::lock;
use crate::blob::BlobStore;
use crate::error::{Error, Result};
use crate::id::BlobId;

/// A [`BlobStore`] kept in memory.
#[derive(Default)]
pub struct MemoryBlobStore {
    blobs: Mutex<HashMap<BlobId, Bytes>>,
}

impl MemoryBlobStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// How many blobs are stored.
    pub fn len(&self) -> usize {
        lock(&self.blobs).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl BlobStore for MemoryBlobStore {
    async fn put(&self, content: Bytes) -> Result<BlobId> {
        let id = BlobId::generate();
        lock(&self.blobs).insert(id, content);
        Ok(id)
    }

    async fn get(&self, id: BlobId) -> Result<Bytes> {
        lock(&self.blobs).get(&id).cloned().ok_or(Error::NotFound)
    }

    async fn delete(&self, id: BlobId) -> Result<()> {
        lock(&self.blobs).remove(&id);
        Ok(())
    }
}
