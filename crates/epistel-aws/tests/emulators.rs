//! The AWS backends against the conformance suites.
//!
//! These need something that speaks the DynamoDB and S3 protocols. They run when the endpoints are
//! named in the environment and report themselves skipped otherwise:
//!
//! | Variable | Example |
//! |---|---|
//! | `EPISTEL_TEST_DYNAMODB_ENDPOINT` | `http://localhost:8000` (DynamoDB Local) |
//! | `EPISTEL_TEST_S3_ENDPOINT` | `http://localhost:9000` (MinIO or any S3-compatible store) |
//!
//! Credentials come from the usual `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`; emulators
//! accept anything, or whatever they were started with. Each run makes its own table and bucket
//! under a random name.

use std::sync::Arc;

use epistel_aws::{DynamoMetadataStore, Location, S3BlobStore};
use epistel_core::{BlobId, conformance};

fn emulator(variable: &str) -> Option<Location> {
    match std::env::var(variable) {
        Ok(endpoint) if !endpoint.is_empty() => {
            let region = std::env::var("AWS_REGION").unwrap_or_else(|_| "us-east-1".to_owned());
            Some(Location {
                region: Some(region),
                endpoint: Some(endpoint),
            })
        }
        _ => {
            eprintln!("skipped: {variable} is not set");
            None
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn dynamodb_metadata_store_conforms() {
    let Some(location) = emulator("EPISTEL_TEST_DYNAMODB_ENDPOINT") else {
        return;
    };
    let client = epistel_aws::dynamodb_client(&location).await;
    let table = format!("epistel-test-{}", BlobId::generate());
    DynamoMetadataStore::create_table(&client, &table)
        .await
        .expect("create table");

    conformance::metadata_store(Arc::new(DynamoMetadataStore::new(client.clone(), &table))).await;

    client
        .delete_table()
        .table_name(&table)
        .send()
        .await
        .expect("delete table");
}

#[tokio::test]
async fn s3_blob_store_conforms() {
    let Some(location) = emulator("EPISTEL_TEST_S3_ENDPOINT") else {
        return;
    };
    let client = epistel_aws::s3_client(&location).await;
    let bucket = format!("epistel-test-{}", BlobId::generate());
    client
        .create_bucket()
        .bucket(&bucket)
        .send()
        .await
        .expect("create bucket");

    conformance::blob_store(&S3BlobStore::new(client.clone(), &bucket, "blobs/")).await;

    let left = client
        .list_objects_v2()
        .bucket(&bucket)
        .send()
        .await
        .expect("list bucket");
    assert_eq!(left.key_count(), Some(0), "the suite cleans up after itself");
    client
        .delete_bucket()
        .bucket(&bucket)
        .send()
        .await
        .expect("delete bucket");
}
