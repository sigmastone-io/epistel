// SPDX-FileCopyrightText: 2026 Sigmastone OÜ
// SPDX-License-Identifier: Apache-2.0

//! Turns the backend sections of the configuration into trait objects. This is the only place
//! that knows which implementations exist.

use std::sync::Arc;

use epistel_aws::{DynamoMetadataStore, Location, S3BlobStore, SesOutboundRelay, SesSqsInboundSource};
use epistel_core::memory::{
    MemoryBlobStore, MemoryChangeBus, MemoryInboundSource, MemoryMetadataStore, MemoryOutboundRelay,
};
use epistel_core::{BlobStore, ChangeBus, InboundSource, MetadataStore, OutboundRelay};

use crate::config::{BlobBackend, Config, InboundBackend, MetadataBackend, OutboundBackend};

/// One implementation of every backend trait, as the configuration asked for.
///
pub struct Backends {
    pub metadata: Arc<dyn MetadataStore>,
    pub blobs: Arc<dyn BlobStore>,
    pub inbound: Arc<dyn InboundSource>,
    pub outbound: Arc<dyn OutboundRelay>,
    pub changes: Arc<dyn ChangeBus>,
}

impl Backends {
    /// Builds the backends. Cloud backends find their credentials here but do not call out yet;
    /// a wrong table or bucket shows on first use.
    pub async fn build(config: &Config) -> Self {
        let metadata: Arc<dyn MetadataStore> = match &config.metadata {
            MetadataBackend::Memory {} => Arc::new(MemoryMetadataStore::new()),
            MetadataBackend::Dynamodb { table, region, endpoint } => {
                let location = Location { region: region.clone(), endpoint: endpoint.clone() };
                Arc::new(DynamoMetadataStore::new(epistel_aws::dynamodb_client(&location).await, table))
            }
        };
        let blobs: Arc<dyn BlobStore> = match &config.blobs {
            BlobBackend::Memory {} => Arc::new(MemoryBlobStore::new()),
            BlobBackend::S3 { bucket, prefix, region, endpoint } => {
                let location = Location { region: region.clone(), endpoint: endpoint.clone() };
                Arc::new(S3BlobStore::new(epistel_aws::s3_client(&location).await, bucket, prefix))
            }
        };
        let inbound: Arc<dyn InboundSource> = match &config.inbound {
            InboundBackend::Memory {} => Arc::new(MemoryInboundSource::new()),
            InboundBackend::SesSqs { queue_url, region, endpoint, s3_endpoint } => {
                let queues = Location { region: region.clone(), endpoint: endpoint.clone() };
                let buckets = Location { region: region.clone(), endpoint: s3_endpoint.clone() };
                Arc::new(SesSqsInboundSource::new(
                    epistel_aws::sqs_client(&queues).await,
                    epistel_aws::s3_client(&buckets).await,
                    queue_url,
                ))
            }
        };
        let outbound: Arc<dyn OutboundRelay> = match &config.outbound {
            OutboundBackend::Memory {} => Arc::new(MemoryOutboundRelay::default()),
            OutboundBackend::Ses { region, endpoint, configuration_set } => {
                let location = Location { region: region.clone(), endpoint: endpoint.clone() };
                Arc::new(SesOutboundRelay::new(epistel_aws::ses_client(&location).await, configuration_set.clone()))
            }
        };
        Self {
            metadata,
            blobs,
            inbound,
            outbound,
            // One instance, so the bus inside the process is the whole bus. It becomes a
            // configuration section when there is a second implementation to choose.
            changes: Arc::new(MemoryChangeBus::new()),
        }
    }
}
