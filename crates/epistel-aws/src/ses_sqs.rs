//! Mail received by Amazon SES, picked up from an SQS queue.
//!
//! The expected setup: an SES receipt rule with one S3 action that stores the raw message and
//! publishes its receipt notification to an SNS topic; the topic feeds an SQS queue; the queue
//! has a redrive policy to a dead-letter queue. This source reads the queue, takes envelope
//! recipients and verdicts from the notification, and fetches the raw message from the bucket
//! the notification names.
//!
//! Whether the topic delivers raw or wrapped in its own envelope does not matter; both are read.
//! A queue message that is not a usable notification is left alone, so that the redrive policy
//! moves it to the dead-letter queue where someone can look at it.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use aws_sdk_sqs::types::MessageSystemAttributeName;
use bytes::Bytes;
use epistel_core::{EmailAddress, Error, InboundMessage, InboundSource, Result, Verdict, Verdicts};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::fault;

/// An [`InboundSource`] for SES receipt notifications arriving over SQS.
pub struct SesSqsInboundSource {
    sqs: aws_sdk_sqs::Client,
    s3: aws_sdk_s3::Client,
    queue_url: String,
}

/// The part of an SES receipt notification this source needs.
#[derive(Deserialize)]
struct Notification {
    mail: Mail,
    receipt: Receipt,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Mail {
    message_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Receipt {
    recipients: Vec<String>,
    spam_verdict: Option<Status>,
    virus_verdict: Option<Status>,
    spf_verdict: Option<Status>,
    dkim_verdict: Option<Status>,
    dmarc_verdict: Option<Status>,
    action: Action,
}

#[derive(Deserialize)]
struct Status {
    status: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Action {
    #[serde(rename = "type")]
    kind: String,
    bucket_name: Option<String>,
    object_key: Option<String>,
}

/// What it takes to fetch and to acknowledge one delivery. Travels in [`InboundMessage::receipt`].
#[derive(Serialize, Deserialize)]
struct Handle {
    bucket: String,
    key: String,
    receipt_handle: String,
}

fn verdict(status: &Option<Status>) -> Verdict {
    // SES also reports GRAY and PROCESSING_FAILED; neither is a clear answer.
    match status.as_ref().map(|status| status.status.as_str()) {
        Some("PASS") => Verdict::Pass,
        Some("FAIL") => Verdict::Fail,
        _ => Verdict::Unknown,
    }
}

/// Reads a queue message body. `Err` says why it is not a notification this source can use.
fn parse(body: &str, receipt_handle: &str, received: SystemTime) -> std::result::Result<InboundMessage, String> {
    let mut value: serde_json::Value = serde_json::from_str(body).map_err(|error| format!("not JSON: {error}"))?;
    // Without raw message delivery, SNS wraps the notification in an envelope of its own.
    if value.get("Type").and_then(|kind| kind.as_str()) == Some("Notification") {
        let inner =
            value.get("Message").and_then(|message| message.as_str()).ok_or("SNS envelope without a message")?;
        value = serde_json::from_str(inner).map_err(|error| format!("SNS message is not JSON: {error}"))?;
    }
    let notification: Notification =
        serde_json::from_value(value).map_err(|error| format!("not a receipt notification: {error}"))?;
    let action = notification.receipt.action;
    let (Some(bucket), Some(key)) = (action.bucket_name, action.object_key) else {
        return Err(format!("the receipt rule's action is {}, not S3 with a bucket and key", action.kind));
    };
    let mut recipients = Vec::new();
    for recipient in &notification.receipt.recipients {
        match EmailAddress::parse(recipient) {
            Ok(address) => recipients.push(address),
            Err(_) => warn!(mail = %notification.mail.message_id, "skipped a recipient that is not an address"),
        }
    }
    let handle = Handle { bucket, key, receipt_handle: receipt_handle.to_owned() };
    Ok(InboundMessage {
        id: notification.mail.message_id,
        recipients,
        verdicts: Verdicts {
            spam: verdict(&notification.receipt.spam_verdict),
            virus: verdict(&notification.receipt.virus_verdict),
            spf: verdict(&notification.receipt.spf_verdict),
            dkim: verdict(&notification.receipt.dkim_verdict),
            dmarc: verdict(&notification.receipt.dmarc_verdict),
        },
        received,
        receipt: serde_json::to_string(&handle).expect("a handle is plain text fields"),
    })
}

fn handle_of(message: &InboundMessage) -> Result<Handle> {
    serde_json::from_str(&message.receipt).map_err(|_| Error::unavailable("the message did not come from this source"))
}

impl SesSqsInboundSource {
    pub fn new(sqs: aws_sdk_sqs::Client, s3: aws_sdk_s3::Client, queue_url: impl Into<String>) -> Self {
        Self { sqs, s3, queue_url: queue_url.into() }
    }
}

#[async_trait]
impl InboundSource for SesSqsInboundSource {
    async fn receive(&self, max: usize, wait: Duration) -> Result<Vec<InboundMessage>> {
        if max == 0 {
            return Ok(Vec::new());
        }
        // SQS hands out at most ten messages and waits at most twenty seconds per call.
        let output = self
            .sqs
            .receive_message()
            .queue_url(&self.queue_url)
            .max_number_of_messages(max.min(10) as i32)
            .wait_time_seconds(wait.as_secs().min(20) as i32)
            .message_system_attribute_names(MessageSystemAttributeName::SentTimestamp)
            .send()
            .await
            .map_err(fault)?;
        let mut messages = Vec::new();
        for queued in output.messages() {
            let (Some(body), Some(receipt_handle)) = (queued.body(), queued.receipt_handle()) else { continue };
            let sent =
                queued.attributes().and_then(|attributes| attributes.get(&MessageSystemAttributeName::SentTimestamp));
            let received = sent
                .and_then(|millis| millis.parse::<u64>().ok())
                .map_or_else(SystemTime::now, |millis| UNIX_EPOCH + Duration::from_millis(millis));
            match parse(body, receipt_handle, received) {
                Ok(message) => messages.push(message),
                Err(reason) => warn!(
                    queue_message = queued.message_id().unwrap_or("unknown"),
                    reason, "left an unusable queue message for the dead-letter queue"
                ),
            }
        }
        Ok(messages)
    }

    async fn fetch(&self, message: &InboundMessage) -> Result<Bytes> {
        let handle = handle_of(message)?;
        let output = match self.s3.get_object().bucket(&handle.bucket).key(&handle.key).send().await {
            Ok(output) => output,
            Err(error) if error.as_service_error().is_some_and(|e| e.is_no_such_key()) => return Err(Error::NotFound),
            Err(error) => return Err(fault(error)),
        };
        Ok(output.body.collect().await.map_err(fault)?.into_bytes())
    }

    async fn acknowledge(&self, message: &InboundMessage) -> Result<()> {
        let handle = handle_of(message)?;
        // The queue first: once the message is gone from it, nothing offers it again. If the
        // object then survives a failure here, the bucket's lifecycle rule removes it.
        let deleted =
            self.sqs.delete_message().queue_url(&self.queue_url).receipt_handle(&handle.receipt_handle).send().await;
        match deleted {
            Ok(_) => {}
            // The handle no longer names a delivery: the message was deleted before, or its time
            // ran out and it was handed out again. Either way there is nothing left to delete
            // under this handle, and a message handed out again is recognized when it is filed.
            Err(error) if error.as_service_error().is_some_and(|e| e.is_receipt_handle_is_invalid()) => {}
            Err(error) => return Err(fault(error)),
        }
        self.s3.delete_object().bucket(&handle.bucket).key(&handle.key).send().await.map_err(fault)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A receipt notification as SES publishes it, shortened to what matters and a little more.
    const NOTIFICATION: &str = r#"{
        "notificationType": "Received",
        "mail": {
            "timestamp": "2026-01-05T10:00:00.000Z",
            "source": "someone@elsewhere.example",
            "messageId": "o3vrnil0e2ic28trm7dfhrc2v0clambda4nbp0g1",
            "destination": ["Ada@Example.org"],
            "headersTruncated": false,
            "commonHeaders": {"subject": "hello"}
        },
        "receipt": {
            "timestamp": "2026-01-05T10:00:00.000Z",
            "processingTimeMillis": 406,
            "recipients": ["Ada@Example.org", "sales@example.org"],
            "spamVerdict": {"status": "FAIL"},
            "virusVerdict": {"status": "PASS"},
            "spfVerdict": {"status": "GRAY"},
            "dkimVerdict": {"status": "PASS"},
            "dmarcVerdict": {"status": "PROCESSING_FAILED"},
            "action": {
                "type": "S3",
                "topicArn": "arn:aws:sns:eu-west-1:123456789012:epistel-inbound",
                "bucketName": "example-epistel-mail",
                "objectKeyPrefix": "incoming",
                "objectKey": "incoming/o3vrnil0e2ic28trm7dfhrc2v0clambda4nbp0g1"
            }
        }
    }"#;

    fn parsed(body: &str) -> std::result::Result<InboundMessage, String> {
        parse(body, "receipt-handle", UNIX_EPOCH)
    }

    #[test]
    fn a_notification_gives_recipients_verdicts_and_where_the_message_is() {
        let message = parsed(NOTIFICATION).unwrap();
        assert_eq!(message.id, "o3vrnil0e2ic28trm7dfhrc2v0clambda4nbp0g1");
        let recipients: Vec<_> = message.recipients.iter().map(EmailAddress::as_str).collect();
        assert_eq!(recipients, ["ada@example.org", "sales@example.org"], "the envelope recipients, not the To header");
        assert_eq!(
            message.verdicts,
            Verdicts {
                spam: Verdict::Fail,
                virus: Verdict::Pass,
                spf: Verdict::Unknown,
                dkim: Verdict::Pass,
                dmarc: Verdict::Unknown
            }
        );
        let handle = handle_of(&message).unwrap();
        assert_eq!(handle.bucket, "example-epistel-mail");
        assert_eq!(handle.key, "incoming/o3vrnil0e2ic28trm7dfhrc2v0clambda4nbp0g1");
        assert_eq!(handle.receipt_handle, "receipt-handle");
    }

    #[test]
    fn the_sns_envelope_is_unwrapped() {
        let envelope = serde_json::json!({
            "Type": "Notification",
            "MessageId": "5b1f1b1c-0000-0000-0000-000000000000",
            "TopicArn": "arn:aws:sns:eu-west-1:123456789012:epistel-inbound",
            "Message": NOTIFICATION,
        });
        assert_eq!(parsed(&envelope.to_string()).unwrap(), parsed(NOTIFICATION).unwrap());
    }

    #[test]
    fn missing_verdicts_are_unknown_and_bad_recipients_are_skipped() {
        let mut value: serde_json::Value = serde_json::from_str(NOTIFICATION).unwrap();
        let receipt = value["receipt"].as_object_mut().unwrap();
        receipt.remove("spamVerdict");
        receipt.insert("recipients".into(), serde_json::json!(["not an address", "ada@example.org"]));
        let message = parsed(&value.to_string()).unwrap();
        assert_eq!(message.verdicts.spam, Verdict::Unknown);
        assert_eq!(message.recipients.len(), 1);
    }

    #[test]
    fn what_is_not_a_usable_notification_is_refused_with_a_reason() {
        assert!(parsed("hello").unwrap_err().contains("not JSON"));
        assert!(parsed(r#"{"Type": "Notification"}"#).unwrap_err().contains("without a message"));
        assert!(parsed(r#"{"Type": "SubscriptionConfirmation"}"#).unwrap_err().contains("not a receipt notification"));
        let other_action = NOTIFICATION
            .replace(r#""type": "S3""#, r#""type": "SNS""#)
            .replace(r#""bucketName": "example-epistel-mail","#, "")
            .replace(r#""objectKey": "incoming/o3vrnil0e2ic28trm7dfhrc2v0clambda4nbp0g1""#, r#""encoding": "UTF8""#);
        assert!(parsed(&other_action).unwrap_err().contains("is SNS, not S3"));
    }
}
