// SPDX-FileCopyrightText: 2026 Sigmastone OÜ
// SPDX-License-Identifier: Apache-2.0

//! Address lists in message headers, such as `From` and `To`.
//!
//! An address comes apart into display name, local part and host, which is the form the IMAP
//! envelope reports and the form a sender check needs. A group shows as a marker with the
//! group's name, its members, and an end marker. Display names are passed on as written,
//! encoded words included; decoding them is the reader's job.

/// One element of an address list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvelopeAddress {
    pub name: Option<String>,
    pub mailbox: Option<String>,
    pub host: Option<String>,
}

#[derive(Default)]
struct Scan {
    /// Text outside angle brackets, quotes removed.
    phrase: String,
    /// Text inside angle brackets.
    angle: Option<String>,
    /// The last comment, which old mail uses for the display name.
    comment: String,
}

impl Scan {
    fn finish(self, out: &mut Vec<EnvelopeAddress>) {
        let phrase = self.phrase.split_whitespace().collect::<Vec<_>>().join(" ");
        let (address, name) = match self.angle {
            Some(address) => (address, phrase),
            None => (phrase, self.comment.trim().to_owned()),
        };
        // A source route, `@a,@b:user@host`, is obsolete; only the address after it matters.
        let address = address.rsplit(':').next().unwrap_or_default().trim().to_owned();
        if address.is_empty() && name.is_empty() {
            return;
        }
        let (mailbox, host) = match address.rsplit_once('@') {
            Some((local, host)) => (Some(local.to_owned()), Some(host.to_owned())),
            None if address.is_empty() => (None, None),
            None => (Some(address), None),
        };
        out.push(EnvelopeAddress { name: (!name.is_empty()).then_some(name), mailbox, host });
    }
}

/// Parses the value of an address header, such as `To`.
pub fn parse_address_list(value: &str) -> Vec<EnvelopeAddress> {
    let mut out = Vec::new();
    let mut scan = Scan::default();
    let mut in_group = false;
    let mut quoted = false;
    let mut escaped = false;
    let mut in_angle = false;
    let mut comment_depth = 0usize;

    for c in value.chars() {
        if escaped {
            escaped = false;
            if comment_depth > 0 {
                scan.comment.push(c);
            } else {
                scan.phrase.push(c);
            }
            continue;
        }
        if comment_depth > 0 {
            match c {
                '\\' => escaped = true,
                '(' => comment_depth += 1,
                ')' => comment_depth -= 1,
                _ => scan.comment.push(c),
            }
            continue;
        }
        if quoted {
            match c {
                '\\' => escaped = true,
                '"' => quoted = false,
                _ if in_angle => scan.angle.get_or_insert_default().push(c),
                _ => scan.phrase.push(c),
            }
            continue;
        }
        match c {
            '"' => quoted = true,
            '(' => {
                comment_depth = 1;
                scan.comment.clear();
            }
            '<' => {
                in_angle = true;
                scan.angle = Some(String::new());
            }
            '>' => in_angle = false,
            ',' if !in_angle => std::mem::take(&mut scan).finish(&mut out),
            ':' if !in_angle && !in_group => {
                // What came before the colon names a group.
                let name = std::mem::take(&mut scan).phrase.trim().to_owned();
                out.push(EnvelopeAddress { name: None, mailbox: Some(name), host: None });
                in_group = true;
            }
            ';' if in_group && !in_angle => {
                std::mem::take(&mut scan).finish(&mut out);
                out.push(EnvelopeAddress { name: None, mailbox: None, host: None });
                in_group = false;
            }
            _ if in_angle => scan.angle.get_or_insert_default().push(c),
            _ => scan.phrase.push(c),
        }
    }
    scan.finish(&mut out);
    if in_group {
        out.push(EnvelopeAddress { name: None, mailbox: None, host: None });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(name: Option<&str>, mailbox: &str, host: &str) -> EnvelopeAddress {
        EnvelopeAddress {
            name: name.map(str::to_owned),
            mailbox: Some(mailbox.to_owned()),
            host: Some(host.to_owned()),
        }
    }

    #[test]
    fn the_common_forms_are_understood() {
        let parsed = parse_address_list(
            r#"Ada Lovelace <ada@example.org>, bob@example.org, "Smith, John" <john@example.org>, carol@example.org (Carol C)"#,
        );
        assert_eq!(
            parsed,
            [
                address(Some("Ada Lovelace"), "ada", "example.org"),
                address(None, "bob", "example.org"),
                address(Some("Smith, John"), "john", "example.org"),
                address(Some("Carol C"), "carol", "example.org"),
            ]
        );
    }

    #[test]
    fn encoded_words_and_escapes_pass_through() {
        let parsed = parse_address_list(r#"=?utf-8?B?w4VrZQ==?= <ake@example.org>, "a \"b\" c" <x@example.org>"#);
        assert_eq!(parsed[0].name.as_deref(), Some("=?utf-8?B?w4VrZQ==?="));
        assert_eq!(parsed[1].name.as_deref(), Some("a \"b\" c"));
    }

    #[test]
    fn groups_are_bracketed_by_markers() {
        let parsed = parse_address_list("Team: ada@example.org, Bob <bob@example.org>;, eve@example.org");
        let marker = |name: Option<&str>| EnvelopeAddress { name: None, mailbox: name.map(str::to_owned), host: None };
        assert_eq!(
            parsed,
            [
                marker(Some("Team")),
                address(None, "ada", "example.org"),
                address(Some("Bob"), "bob", "example.org"),
                marker(None),
                address(None, "eve", "example.org"),
            ]
        );
        assert_eq!(
            parse_address_list("undisclosed-recipients:;"),
            [marker(Some("undisclosed-recipients")), marker(None)]
        );
    }

    #[test]
    fn odd_input_does_not_break_anything() {
        assert!(parse_address_list("").is_empty());
        assert!(parse_address_list(" , ,").is_empty());
        assert_eq!(parse_address_list("<@relay.example:ada@example.org>"), [address(None, "ada", "example.org")]);
        assert_eq!(parse_address_list("postmaster")[0].host, None);
        assert_eq!(parse_address_list("Ada <ada@example.org")[0], address(Some("Ada"), "ada", "example.org"));
    }
}
