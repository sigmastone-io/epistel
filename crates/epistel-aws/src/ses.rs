// SPDX-FileCopyrightText: 2026 Sigmastone OÜ
// SPDX-License-Identifier: Apache-2.0

//! Sending through Amazon SES.
//!
//! The message goes out through the SES v2 send API as raw content, under the identity of the
//! machine: there are no SMTP credentials to keep. SES signs it, delivers it, retries, and turns
//! failures into bounce mail that comes back through the inbound path.

use async_trait::async_trait;
use aws_sdk_sesv2::Client;
use aws_sdk_sesv2::error::ProvideErrorMetadata;
use aws_sdk_sesv2::operation::send_email::SendEmailError;
use aws_sdk_sesv2::primitives::Blob;
use aws_sdk_sesv2::types::{Destination, EmailContent, RawMessage};
use epistel_core::{OutboundMessage, OutboundRelay, RelayError, RelayLimits};

/// What SES takes in one call: a message of 40 megabytes and fifty recipients.
const LIMITS: RelayLimits = RelayLimits { max_message_size: 40_000_000, max_recipients: 50 };

/// An [`OutboundRelay`] on the SES v2 send API.
pub struct SesOutboundRelay {
    client: Client,
    configuration_set: Option<String>,
}

impl SesOutboundRelay {
    /// `configuration_set` names the SES configuration set to send under, if any.
    pub fn new(client: Client, configuration_set: Option<String>) -> Self {
        Self { client, configuration_set }
    }
}

/// Whether SES will say the same again. Everything else may pass on a later attempt.
fn permanent(error: &SendEmailError) -> bool {
    error.is_message_rejected()
        || error.is_mail_from_domain_not_verified_exception()
        || error.is_account_suspended_exception()
        || error.is_sending_paused_exception()
        || error.is_bad_request_exception()
        || error.is_not_found_exception()
}

#[async_trait]
impl OutboundRelay for SesOutboundRelay {
    fn limits(&self) -> RelayLimits {
        LIMITS
    }

    async fn send(&self, message: OutboundMessage) -> Result<(), RelayError> {
        if message.raw.len() as u64 > LIMITS.max_message_size {
            return Err(RelayError::TooLarge);
        }
        if message.recipients.is_empty() || message.recipients.len() > LIMITS.max_recipients {
            return Err(RelayError::RecipientCount);
        }
        let raw = RawMessage::builder()
            .data(Blob::new(message.raw.to_vec()))
            .build()
            .map_err(|error| RelayError::Transient(error.into()))?;
        // With raw content the destination is the envelope: these addresses get the message,
        // whatever its headers say, which is how recipients named only in Bcc are reached.
        let recipients = message.recipients.iter().map(|address| address.as_str().to_owned()).collect();
        let sent = self
            .client
            .send_email()
            .from_email_address(message.sender.as_str())
            .destination(Destination::builder().set_to_addresses(Some(recipients)).build())
            .content(EmailContent::builder().raw(raw).build())
            .set_configuration_set_name(self.configuration_set.clone())
            .send()
            .await;
        match sent {
            Ok(_) => Ok(()),
            Err(error) => match error.as_service_error() {
                Some(service) if permanent(service) => Err(RelayError::Rejected(
                    service.message().unwrap_or("the mail service refused the message").to_owned(),
                )),
                // Throttling, quota, and whatever went wrong on the way.
                _ => Err(RelayError::Transient(error.into())),
            },
        }
    }
}
