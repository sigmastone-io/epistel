use bytes::Bytes;

use crate::address::EmailAddress;
use crate::outbound::{OutboundMessage, OutboundRelay, RelayError};

/// Runs the [`OutboundRelay`] contract against `relay`.
///
/// One small message really is sent from `sender` to `recipient`, so against a real mail service
/// both have to be addresses that service lets the caller use.
pub async fn outbound_relay(relay: &dyn OutboundRelay, sender: EmailAddress, recipient: EmailAddress) {
    let limits = relay.limits();
    assert!(limits.max_message_size > 0 && limits.max_recipients > 0, "limits allow something");

    let raw = format!("From: {sender}\r\nTo: {recipient}\r\nSubject: Epistel conformance\r\n\r\nSent by a test.\r\n");
    let message = OutboundMessage { sender, recipients: vec![recipient.clone()], raw: Bytes::from(raw) };
    relay.send(message.clone()).await.expect("a message within the limits is accepted");

    let nobody = OutboundMessage { recipients: Vec::new(), ..message.clone() };
    assert!(matches!(relay.send(nobody).await, Err(RelayError::RecipientCount)), "no recipients is refused");

    let crowd = OutboundMessage { recipients: vec![recipient; limits.max_recipients + 1], ..message.clone() };
    assert!(matches!(relay.send(crowd).await, Err(RelayError::RecipientCount)), "too many recipients is refused");

    // Building the oversized message costs memory in proportion to the limit; skip it where the
    // limit is unreasonably large to allocate.
    if limits.max_message_size <= 64 * 1024 * 1024 {
        let size = usize::try_from(limits.max_message_size).expect("fits in memory") + 1;
        let huge = OutboundMessage { raw: Bytes::from(vec![b'x'; size]), ..message };
        assert!(matches!(relay.send(huge).await, Err(RelayError::TooLarge)), "an oversized message is refused");
    }
}
