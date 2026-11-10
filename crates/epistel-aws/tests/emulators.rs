//! The AWS backends against the conformance suites.
//!
//! These need something that speaks the DynamoDB and S3 protocols. They run when the endpoints are
//! named in the environment and report themselves skipped otherwise:
//!
//! | Variable | Example |
//! |---|---|
//! | `EPISTEL_TEST_DYNAMODB_ENDPOINT` | `http://localhost:8000` (DynamoDB Local) |
//! | `EPISTEL_TEST_S3_ENDPOINT` | `http://localhost:9090` (S3Mock or any S3-compatible store) |
//! | `EPISTEL_TEST_SQS_ENDPOINT` | `http://localhost:9324` (ElasticMQ) |
//!
//! Credentials come from the usual `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`; emulators
//! accept anything, or whatever they were started with. Each run makes its own table and bucket
//! under a random name.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use aws_sdk_s3::primitives::ByteStream;
use bytes::Bytes;
use epistel_aws::{DynamoMetadataStore, Location, S3BlobStore, SesSqsInboundSource};
use epistel_core::conformance::InboundFixture;
use epistel_core::{BlobId, EmailAddress, InboundSource, Verdict, Verdicts, conformance};

fn emulator(variable: &str) -> Option<Location> {
    match std::env::var(variable) {
        Ok(endpoint) if !endpoint.is_empty() => {
            let region = std::env::var("AWS_REGION").unwrap_or_else(|_| "us-east-1".to_owned());
            Some(Location { region: Some(region), endpoint: Some(endpoint) })
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
    DynamoMetadataStore::create_table(&client, &table).await.expect("create table");

    conformance::metadata_store(Arc::new(DynamoMetadataStore::new(client.clone(), &table))).await;

    client.delete_table().table_name(&table).send().await.expect("delete table");
}

#[tokio::test]
async fn s3_blob_store_conforms() {
    let Some(location) = emulator("EPISTEL_TEST_S3_ENDPOINT") else {
        return;
    };
    let client = epistel_aws::s3_client(&location).await;
    let bucket = format!("epistel-test-{}", BlobId::generate());
    client.create_bucket().bucket(&bucket).send().await.expect("create bucket");

    conformance::blob_store(&S3BlobStore::new(client.clone(), &bucket, "blobs/")).await;

    let left = client.list_objects_v2().bucket(&bucket).send().await.expect("list bucket");
    assert_eq!(left.key_count(), Some(0), "the suite cleans up after itself");
    client.delete_bucket().bucket(&bucket).send().await.expect("delete bucket");
}

/// Does what SES does when mail arrives: stores the raw message in the bucket and puts the
/// receipt notification on the queue.
struct SesStandIn {
    sqs: aws_sdk_sqs::Client,
    s3: aws_sdk_s3::Client,
    queue_url: String,
    bucket: String,
}

fn status(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Pass => "PASS",
        Verdict::Fail => "FAIL",
        Verdict::Unknown => "GRAY",
    }
}

#[async_trait]
impl InboundFixture for SesStandIn {
    async fn deliver(&self, recipients: Vec<EmailAddress>, verdicts: Verdicts, raw: Bytes) {
        let id = BlobId::generate().to_string();
        let key = format!("incoming/{id}");
        self.s3.put_object().bucket(&self.bucket).key(&key).body(ByteStream::from(raw)).send().await.expect("store");
        let recipients: Vec<&str> = recipients.iter().map(EmailAddress::as_str).collect();
        let notification = serde_json::json!({
            "notificationType": "Received",
            "mail": {"messageId": id, "source": "someone@elsewhere.example", "destination": recipients},
            "receipt": {
                "recipients": recipients,
                "spamVerdict": {"status": status(verdicts.spam)},
                "virusVerdict": {"status": status(verdicts.virus)},
                "spfVerdict": {"status": status(verdicts.spf)},
                "dkimVerdict": {"status": status(verdicts.dkim)},
                "dmarcVerdict": {"status": status(verdicts.dmarc)},
                "action": {"type": "S3", "bucketName": self.bucket, "objectKey": key},
            },
        });
        let body = notification.to_string();
        self.sqs.send_message().queue_url(&self.queue_url).message_body(body).send().await.expect("notify");
    }
}

#[tokio::test]
async fn ses_sqs_inbound_source_conforms() {
    let (Some(queues), Some(buckets)) = (emulator("EPISTEL_TEST_SQS_ENDPOINT"), emulator("EPISTEL_TEST_S3_ENDPOINT"))
    else {
        return;
    };
    let sqs = epistel_aws::sqs_client(&queues).await;
    let s3 = epistel_aws::s3_client(&buckets).await;
    let name = format!("epistel-test-{}", BlobId::generate());
    s3.create_bucket().bucket(&name).send().await.expect("create bucket");
    let queue = sqs.create_queue().queue_name(&name).send().await.expect("create queue");
    let queue_url = queue.queue_url().expect("the new queue has a URL").to_owned();

    // Something on the queue that is no notification must not stop the mail behind it.
    sqs.send_message().queue_url(&queue_url).message_body("not a notification").send().await.expect("send");
    let source = SesSqsInboundSource::new(sqs.clone(), s3.clone(), &queue_url);
    assert!(source.receive(10, Duration::from_secs(1)).await.expect("receive").is_empty());
    sqs.purge_queue().queue_url(&queue_url).send().await.expect("purge");

    let ses = SesStandIn { sqs: sqs.clone(), s3: s3.clone(), queue_url: queue_url.clone(), bucket: name.clone() };
    conformance::inbound_source(&source, &ses, Duration::from_secs(5)).await;

    let left = s3.list_objects_v2().bucket(&name).send().await.expect("list bucket");
    assert_eq!(left.key_count(), Some(0), "acknowledged mail is removed from the bucket");
    sqs.delete_queue().queue_url(&queue_url).send().await.expect("delete queue");
    s3.delete_bucket().bucket(&name).send().await.expect("delete bucket");
}
