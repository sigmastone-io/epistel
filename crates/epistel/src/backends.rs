//! Turns the backend sections of the configuration into trait objects. This is the only place
//! that knows which implementations exist.

use std::sync::Arc;

use epistel_core::memory::{
    MemoryBlobStore, MemoryChangeBus, MemoryInboundSource, MemoryMetadataStore, MemoryOutboundRelay,
};
use epistel_core::{BlobStore, ChangeBus, InboundSource, MetadataStore, OutboundRelay};

use crate::config::{BlobBackend, Config, InboundBackend, MetadataBackend, OutboundBackend};

/// One implementation of every backend trait, as the configuration asked for.
///
/// Only `check` exists so far and it looks at the relay's limits; the listeners and the ingest
/// loop that use the rest arrive with their milestones.
#[allow(dead_code)]
pub struct Backends {
    pub metadata: Arc<dyn MetadataStore>,
    pub blobs: Arc<dyn BlobStore>,
    pub inbound: Arc<dyn InboundSource>,
    pub outbound: Arc<dyn OutboundRelay>,
    pub changes: Arc<dyn ChangeBus>,
}

impl Backends {
    pub fn build(config: &Config) -> Self {
        Self {
            metadata: match config.metadata {
                MetadataBackend::Memory {} => Arc::new(MemoryMetadataStore::new()),
            },
            blobs: match config.blobs {
                BlobBackend::Memory {} => Arc::new(MemoryBlobStore::new()),
            },
            inbound: match config.inbound {
                InboundBackend::Memory {} => Arc::new(MemoryInboundSource::new()),
            },
            outbound: match config.outbound {
                OutboundBackend::Memory {} => Arc::new(MemoryOutboundRelay::default()),
            },
            // One instance, so the bus inside the process is the whole bus. It becomes a
            // configuration section when there is a second implementation to choose.
            changes: Arc::new(MemoryChangeBus::new()),
        }
    }
}
