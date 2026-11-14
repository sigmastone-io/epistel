// SPDX-FileCopyrightText: 2026 Sigmastone OÜ
// SPDX-License-Identifier: Apache-2.0

//! What the server looks at and changes in a submitted message: who it says it is from, and
//! the header fields a submission server is expected to see to.

use std::time::SystemTime;

use epistel_core::header_address::parse_address_list;
use epistel_core::{BlobId, EmailAddress, mime};

/// The addresses in the `From` header. `None` when there is no such header or it names no
/// usable address, in which case the message is not fit to send.
pub fn authors(raw: &[u8]) -> Option<Vec<EmailAddress>> {
    let headers = mime::parse(raw);
    let listed = parse_address_list(&headers.values("From").collect::<Vec<_>>().join(", "));
    let mut authors = Vec::new();
    for entry in listed {
        // Group markers have no host; a group in From is not something to send under.
        let (Some(mailbox), Some(host)) = (entry.mailbox, entry.host) else { return None };
        authors.push(EmailAddress::parse(&format!("{mailbox}@{host}")).ok()?);
    }
    (!authors.is_empty()).then_some(authors)
}

/// The message as it is passed on: without `Bcc`, and with `Date` and `Message-ID` if the client
/// left them out. Everything else stays byte for byte.
pub fn prepare(raw: &[u8], hostname: &str) -> Vec<u8> {
    let headers = mime::parse(raw);
    let mut out = Vec::with_capacity(raw.len() + 128);
    if headers.field("Date").is_none() {
        let now = chrono::DateTime::<chrono::Utc>::from(SystemTime::now());
        out.extend_from_slice(format!("Date: {}\r\n", now.format("%a, %d %b %Y %H:%M:%S +0000")).as_bytes());
    }
    if headers.field("Message-ID").is_none() {
        out.extend_from_slice(format!("Message-ID: <{}@{hostname}>\r\n", BlobId::generate()).as_bytes());
    }
    // Recipients named only in Bcc are in the envelope; the header would show them to the others.
    let mut copied = 0;
    for field in headers.fields.iter().filter(|field| field.name.eq_ignore_ascii_case("Bcc")) {
        out.extend_from_slice(&raw[copied..field.range.start]);
        copied = field.range.end;
    }
    out.extend_from_slice(&raw[copied..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(raw: &[u8]) -> String {
        String::from_utf8_lossy(raw).into_owned()
    }

    #[test]
    fn authors_are_the_addresses_in_from() {
        let named = authors(b"From: Ada <Ada@Example.org>, bob@example.org\r\n\r\n").unwrap();
        assert_eq!(named.iter().map(EmailAddress::as_str).collect::<Vec<_>>(), ["ada@example.org", "bob@example.org"]);
        assert_eq!(authors(b"Subject: no author\r\n\r\n"), None);
        assert_eq!(authors(b"From: nobody in particular\r\n\r\n"), None);
        assert_eq!(authors(b"From: Team: ada@example.org;\r\n\r\n"), None);
    }

    #[test]
    fn bcc_is_removed_and_the_rest_is_untouched() {
        let raw = b"From: ada@example.org\r\nBcc: secret@example.org,\r\n other@example.org\r\nTo: bob@example.org\r\n\
Date: Mon, 5 Jan 2026 10:00:00 +0000\r\nMessage-ID: <1@example.org>\r\nbcc: third@example.org\r\n\r\nBcc: in the body\r\n";
        let prepared = text(&prepare(raw, "mail.example.org"));
        assert_eq!(
            prepared,
            "From: ada@example.org\r\nTo: bob@example.org\r\nDate: Mon, 5 Jan 2026 10:00:00 +0000\r\n\
Message-ID: <1@example.org>\r\n\r\nBcc: in the body\r\n"
        );
    }

    #[test]
    fn date_and_message_id_are_added_only_when_missing() {
        let prepared = text(&prepare(b"From: ada@example.org\r\n\r\nhello\r\n", "mail.example.org"));
        let mut lines = prepared.lines();
        let date = lines.next().unwrap();
        assert!(date.starts_with("Date: ") && date.ends_with(" +0000"), "{date}");
        assert!(chrono::DateTime::parse_from_rfc2822(&date[6..]).is_ok(), "{date}");
        let id = lines.next().unwrap();
        assert!(id.starts_with("Message-ID: <") && id.ends_with("@mail.example.org>"), "{id}");
        assert!(prepared.ends_with("From: ada@example.org\r\n\r\nhello\r\n"));
        assert_ne!(
            prepared,
            text(&prepare(b"From: ada@example.org\r\n\r\nhello\r\n", "mail.example.org")),
            "ids differ"
        );
    }
}
