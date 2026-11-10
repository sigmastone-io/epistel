//! Amazon Web Services backends for Epistel.
//!
//! | Trait | Implementation |
//! |---|---|
//! | `MetadataStore` | [`DynamoMetadataStore`], one DynamoDB table |
//! | `BlobStore` | [`S3BlobStore`], one prefix of an S3 bucket |
//! | `InboundSource` | [`SesSqsInboundSource`], SES receipt notifications from an SQS queue |
//! | `OutboundRelay` | [`SesOutboundRelay`], the SES v2 send API |
//!
//! Nothing here creates infrastructure in a deployment: table and bucket come from the
//! `terraform-aws-epistel` module. [`DynamoMetadataStore::create_table`] exists for tests and for
//! trying things out against a local emulator.

mod dynamo;
mod s3;
mod ses;
mod ses_sqs;

pub use dynamo::DynamoMetadataStore;
pub use s3::S3BlobStore;
pub use ses::SesOutboundRelay;
pub use ses_sqs::SesSqsInboundSource;

use aws_config::{BehaviorVersion, Region, SdkConfig};
use epistel_core::Error;

/// Where to find a service: the region, and for emulators and private endpoints the URL.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Location {
    /// The region. Taken from the environment when absent.
    pub region: Option<String>,
    /// Overrides the service's address. Set for emulators; leave out for AWS itself.
    pub endpoint: Option<String>,
}

/// Loads the shared configuration: region, and credentials from the standard chain (environment,
/// profile, container or instance role). Epistel's own configuration never holds credentials.
pub async fn sdk_config(location: &Location) -> SdkConfig {
    let mut loader = aws_config::defaults(BehaviorVersion::latest());
    if let Some(region) = &location.region {
        loader = loader.region(Region::new(region.clone()));
    }
    if let Some(endpoint) = &location.endpoint {
        loader = loader.endpoint_url(endpoint);
    }
    loader.load().await
}

/// A DynamoDB client for `location`.
pub async fn dynamodb_client(location: &Location) -> aws_sdk_dynamodb::Client {
    aws_sdk_dynamodb::Client::new(&sdk_config(location).await)
}

/// An S3 client for `location`. With an endpoint override the bucket goes into the path, which is
/// the only form emulators and most S3-compatible stores understand.
pub async fn s3_client(location: &Location) -> aws_sdk_s3::Client {
    let config = aws_sdk_s3::config::Builder::from(&sdk_config(location).await)
        .force_path_style(location.endpoint.is_some())
        .build();
    aws_sdk_s3::Client::from_conf(config)
}

/// An SES client for `location`.
pub async fn ses_client(location: &Location) -> aws_sdk_sesv2::Client {
    aws_sdk_sesv2::Client::new(&sdk_config(location).await)
}

/// An SQS client for `location`.
pub async fn sqs_client(location: &Location) -> aws_sdk_sqs::Client {
    aws_sdk_sqs::Client::new(&sdk_config(location).await)
}

/// A failure of the service or of the way to it, as opposed to an answer.
fn fault(error: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::unavailable(error)
}
