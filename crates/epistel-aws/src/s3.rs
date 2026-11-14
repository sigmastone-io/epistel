// SPDX-FileCopyrightText: 2026 Sigmastone OÜ
// SPDX-License-Identifier: Apache-2.0

use async_trait::async_trait;
use aws_sdk_s3::Client;
use aws_sdk_s3::primitives::ByteStream;
use bytes::Bytes;
use epistel_core::{BlobId, BlobStore, Error, Result};

use crate::fault;

/// A [`BlobStore`] in an S3 bucket: one object per blob, named `<prefix><blob id>`.
///
/// Encryption at rest, versioning and lifecycle are properties of the bucket and are set where
/// the bucket is created.
pub struct S3BlobStore {
    client: Client,
    bucket: String,
    prefix: String,
}

impl S3BlobStore {
    pub fn new(client: Client, bucket: impl Into<String>, prefix: impl Into<String>) -> Self {
        Self { client, bucket: bucket.into(), prefix: prefix.into() }
    }

    fn key(&self, id: BlobId) -> String {
        format!("{}{id}", self.prefix)
    }
}

#[async_trait]
impl BlobStore for S3BlobStore {
    async fn put(&self, content: Bytes) -> Result<BlobId> {
        let id = BlobId::generate();
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(self.key(id))
            .content_type("message/rfc822")
            .body(ByteStream::from(content))
            .send()
            .await
            .map_err(fault)?;
        Ok(id)
    }

    async fn get(&self, id: BlobId) -> Result<Bytes> {
        let response = self.client.get_object().bucket(&self.bucket).key(self.key(id)).send().await;
        let output = match response {
            Ok(output) => output,
            Err(error) if error.as_service_error().is_some_and(|e| e.is_no_such_key()) => return Err(Error::NotFound),
            Err(error) => return Err(fault(error)),
        };
        Ok(output.body.collect().await.map_err(fault)?.into_bytes())
    }

    async fn delete(&self, id: BlobId) -> Result<()> {
        // S3 answers success for an object that is not there, which is the behaviour the trait asks for.
        self.client.delete_object().bucket(&self.bucket).key(self.key(id)).send().await.map_err(fault)?;
        Ok(())
    }
}
