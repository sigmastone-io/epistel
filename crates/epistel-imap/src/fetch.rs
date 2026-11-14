// SPDX-FileCopyrightText: 2026 Sigmastone OÜ
// SPDX-License-Identifier: Apache-2.0

//! What FETCH returns: envelope, body structure and sections of a message.

use std::borrow::Cow;
use std::num::NonZeroU32;
use std::time::SystemTime;

use epistel_core::model::{Flags, MessageRecord};
use imap_codec::imap_types::core::AString;
use imap_codec::imap_types::fetch::{MessageDataItemName, Part as PartPath, Section};

use crate::address::{EnvelopeAddress, parse_address_list};
use crate::mime::Part;
use crate::wire::Out;

/// A date and time the way `INTERNALDATE` shows it.
pub fn internal_date(time: SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(time).format("%d-%b-%Y %H:%M:%S +0000").to_string()
}

pub fn write_flags(out: &mut Out, flags: &Flags) {
    out.raw("(");
    let mut first = true;
    let mut names: Vec<&str> = flags.system.names().collect();
    names.extend(flags.keywords.iter().map(|keyword| keyword.as_str()));
    for name in names {
        if !std::mem::take(&mut first) {
            out.sp();
        }
        out.raw(name);
    }
    out.raw(")");
}

// ---------------------------------------------------------------------------------------------
// Envelope
// ---------------------------------------------------------------------------------------------

fn write_addresses(out: &mut Out, addresses: &[EnvelopeAddress]) {
    if addresses.is_empty() {
        out.raw("NIL");
        return;
    }
    out.raw("(");
    for address in addresses {
        out.raw("(").nstring(address.name.as_deref()).raw(" NIL ");
        out.nstring(address.mailbox.as_deref()).sp().nstring(address.host.as_deref()).raw(")");
    }
    out.raw(")");
}

/// The envelope of the message whose headers `part` holds.
pub fn write_envelope(out: &mut Out, part: &Part) {
    let value = |name: &str| part.field(name).map(|field| field.value.as_str());
    let addresses = |name: &str| parse_address_list(&part.values(name).collect::<Vec<_>>().join(", "));
    let from = addresses("From");
    // Sender and Reply-To stand for From when the message does not say otherwise.
    let or_from = |name: &str| {
        let own = addresses(name);
        if own.is_empty() { from.clone() } else { own }
    };

    out.raw("(").nstring(value("Date")).sp().nstring(value("Subject")).sp();
    write_addresses(out, &from);
    for list in [or_from("Sender"), or_from("Reply-To"), addresses("To"), addresses("Cc"), addresses("Bcc")] {
        out.sp();
        write_addresses(out, &list);
    }
    out.sp().nstring(value("In-Reply-To")).sp().nstring(value("Message-ID")).raw(")");
}

// ---------------------------------------------------------------------------------------------
// Body structure
// ---------------------------------------------------------------------------------------------

fn write_parameters(out: &mut Out, parameters: &[(String, String)]) {
    if parameters.is_empty() {
        out.raw("NIL");
        return;
    }
    out.raw("(");
    for (index, (name, value)) in parameters.iter().enumerate() {
        if index > 0 {
            out.sp();
        }
        out.string(name.as_bytes()).sp().string(value.as_bytes());
    }
    out.raw(")");
}

fn write_disposition(out: &mut Out, part: &Part) {
    match &part.disposition {
        Some((kind, parameters)) => {
            out.raw("(").string(kind.as_bytes()).sp();
            write_parameters(out, parameters);
            out.raw(")");
        }
        None => {
            out.raw("NIL");
        }
    }
}

/// The structure of `part`. `extended` adds the fields only `BODYSTRUCTURE` shows.
pub fn write_body_structure(out: &mut Out, part: &Part, extended: bool) {
    out.raw("(");
    if part.is_multipart() {
        if part.children.is_empty() {
            // A multipart needs at least one part on the wire; an empty one stands in.
            out.raw("(\"TEXT\" \"PLAIN\" (\"CHARSET\" \"US-ASCII\") NIL NIL \"7BIT\" 0 0");
            out.raw(if extended { " NIL NIL NIL NIL)" } else { ")" });
        }
        for child in &part.children {
            write_body_structure(out, child, extended);
        }
        out.sp().string(part.subtype.as_bytes());
        if extended {
            out.sp();
            write_parameters(out, &part.parameters);
            out.sp();
            write_disposition(out, part);
            out.raw(" NIL NIL");
        }
    } else {
        // An embedded message that could not be read is reported as opaque data: a message type
        // without its envelope and structure would not parse on the other side.
        let opaque_message = part.media_type == "MESSAGE" && part.subtype == "RFC822" && part.message.is_none();
        let (media_type, subtype) = if opaque_message {
            ("APPLICATION", "OCTET-STREAM")
        } else {
            (part.media_type.as_str(), part.subtype.as_str())
        };
        out.string(media_type.as_bytes()).sp().string(subtype.as_bytes()).sp();
        write_parameters(out, &part.parameters);
        out.sp().nstring(part.id.as_deref()).sp().nstring(part.description.as_deref()).sp();
        out.string(part.encoding.as_bytes()).sp().number((part.end - part.body) as u64);
        if let Some(message) = &part.message {
            out.sp();
            write_envelope(out, message);
            out.sp();
            write_body_structure(out, message, extended);
            out.sp().number(part.lines as u64);
        } else if media_type == "TEXT" {
            out.sp().number(part.lines as u64);
        }
        if extended {
            out.raw(" NIL ");
            write_disposition(out, part);
            out.raw(" NIL NIL");
        }
    }
    out.raw(")");
}

// ---------------------------------------------------------------------------------------------
// Sections
// ---------------------------------------------------------------------------------------------

/// The part a path such as `2.1` names, starting from the message `root`.
fn locate<'p>(root: &'p Part, path: &[NonZeroU32]) -> Option<&'p Part> {
    let mut node = root;
    for (depth, number) in path.iter().enumerate() {
        // Numbers below a `message/rfc822` part count the parts of the message inside it.
        let owner = if depth > 0 { node.message.as_deref().unwrap_or(node) } else { node };
        let index = number.get() as usize - 1;
        node = if owner.children.is_empty() {
            // A message that is not multipart has exactly one part: its body.
            if index != 0 {
                return None;
            }
            owner
        } else {
            owner.children.get(index)?
        };
    }
    Some(node)
}

fn path_of(path: &Option<PartPath>) -> &[NonZeroU32] {
    path.as_ref().map_or(&[], |path| path.0.as_ref())
}

/// The message whose header and text a `HEADER` or `TEXT` section refers to: the message itself
/// without a path, the embedded message of the named part with one.
fn message_at<'p>(root: &'p Part, path: &Option<PartPath>) -> Option<&'p Part> {
    match path {
        None => Some(root),
        Some(path) => locate(root, path.0.as_ref())?.message.as_deref(),
    }
}

fn select_fields<'a>(raw: &'a [u8], message: &Part, names: &[AString<'_>], keep_named: bool) -> Cow<'a, [u8]> {
    let mut selected = Vec::new();
    for field in &message.fields {
        let named = names.iter().any(|name| name.as_ref().eq_ignore_ascii_case(field.name.as_bytes()));
        if named == keep_named {
            selected.extend_from_slice(&raw[field.range.clone()]);
        }
    }
    selected.extend_from_slice(b"\r\n");
    Cow::Owned(selected)
}

/// The bytes a section names, or `None` when the message has no such part.
pub fn section<'a>(raw: &'a [u8], root: &Part, section: Option<&Section<'_>>) -> Option<Cow<'a, [u8]>> {
    let slice = |range: std::ops::Range<usize>| Some(Cow::Borrowed(&raw[range]));
    match section {
        None => slice(0..raw.len()),
        Some(Section::Part(path)) => slice(locate(root, path.0.as_ref())?.body_range()),
        Some(Section::Mime(path)) => slice(locate(root, path.0.as_ref())?.header_range()),
        Some(Section::Header(path)) => slice(message_at(root, path)?.header_range()),
        Some(Section::Text(path)) => slice(message_at(root, path)?.body_range()),
        Some(Section::HeaderFields(path, names)) => {
            Some(select_fields(raw, message_at(root, path)?, names.as_ref(), true))
        }
        Some(Section::HeaderFieldsNot(path, names)) => {
            Some(select_fields(raw, message_at(root, path)?, names.as_ref(), false))
        }
    }
}

/// The section as it is echoed in the response: what goes between the brackets of `BODY[...]`.
pub fn section_name(section: Option<&Section<'_>>) -> String {
    let dotted = |path: &[NonZeroU32]| path.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(".");
    let under = |path: &Option<PartPath>, what: &str| match path_of(path) {
        [] => what.to_owned(),
        path => format!("{}.{what}", dotted(path)),
    };
    let listed = |names: &[AString<'_>]| {
        let mut out = Out::default();
        for (index, name) in names.iter().enumerate() {
            if index > 0 {
                out.sp();
            }
            // Field names are tokens; anything else is quoted so the list stays well formed.
            let bytes = name.as_ref();
            if !bytes.is_empty() && bytes.iter().all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-') {
                out.raw(&String::from_utf8_lossy(bytes).to_ascii_uppercase());
            } else {
                out.string(bytes);
            }
        }
        String::from_utf8_lossy(&out.bytes).into_owned()
    };
    match section {
        None => String::new(),
        Some(Section::Part(path)) => dotted(path.0.as_ref()),
        Some(Section::Mime(path)) => format!("{}.MIME", dotted(path.0.as_ref())),
        Some(Section::Header(path)) => under(path, "HEADER"),
        Some(Section::Text(path)) => under(path, "TEXT"),
        Some(Section::HeaderFields(path, names)) => {
            format!("{} ({})", under(path, "HEADER.FIELDS"), listed(names.as_ref()))
        }
        Some(Section::HeaderFieldsNot(path, names)) => {
            format!("{} ({})", under(path, "HEADER.FIELDS.NOT"), listed(names.as_ref()))
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Items
// ---------------------------------------------------------------------------------------------

/// Whether answering the item takes the message itself, beyond its record.
pub fn needs_message(item: &MessageDataItemName<'_>) -> bool {
    !matches!(
        item,
        MessageDataItemName::Flags
            | MessageDataItemName::InternalDate
            | MessageDataItemName::Rfc822Size
            | MessageDataItemName::Uid
    )
}

/// Whether the item is one this server answers.
pub fn supported(item: &MessageDataItemName<'_>) -> bool {
    !matches!(item, MessageDataItemName::Binary { .. } | MessageDataItemName::BinarySize { .. })
}

fn write_section(out: &mut Out, label: &str, content: Option<Cow<'_, [u8]>>, partial: Option<(u32, NonZeroU32)>) {
    out.raw(label);
    let Some(content) = content else {
        out.raw(" NIL");
        return;
    };
    match partial {
        Some((first, count)) => {
            let start = (first as usize).min(content.len());
            let end = start.saturating_add(count.get() as usize).min(content.len());
            out.raw(&format!("<{first}> ")).literal(&content[start..end]);
        }
        None => {
            out.sp().literal(&content);
        }
    }
}

/// Writes one item of a FETCH response, name and value.
///
/// `message` is the raw message and its structure; it is present whenever [`needs_message`] says
/// so for the item.
pub fn write_item(
    out: &mut Out,
    item: &MessageDataItemName<'_>,
    record: &MessageRecord,
    message: Option<(&[u8], &Part)>,
) {
    let (raw, root) = match message {
        Some((raw, root)) => (raw, Some(root)),
        None => (&[][..], None),
    };
    match item {
        MessageDataItemName::Uid => {
            out.raw("UID ").number(u64::from(record.uid.get()));
        }
        MessageDataItemName::Flags => {
            out.raw("FLAGS ");
            write_flags(out, &record.flags);
        }
        MessageDataItemName::InternalDate => {
            out.raw("INTERNALDATE ").string(internal_date(record.internal_date).as_bytes());
        }
        MessageDataItemName::Rfc822Size => {
            out.raw("RFC822.SIZE ").number(record.size);
        }
        MessageDataItemName::Envelope => {
            out.raw("ENVELOPE ");
            if let Some(root) = root {
                write_envelope(out, root);
            }
        }
        MessageDataItemName::Body | MessageDataItemName::BodyStructure => {
            let extended = matches!(item, MessageDataItemName::BodyStructure);
            out.raw(if extended { "BODYSTRUCTURE " } else { "BODY " });
            if let Some(root) = root {
                write_body_structure(out, root, extended);
            }
        }
        MessageDataItemName::BodyExt { section: wanted, partial, .. } => {
            let label = format!("BODY[{}]", section_name(wanted.as_ref()));
            write_section(out, &label, root.and_then(|root| section(raw, root, wanted.as_ref())), *partial);
        }
        MessageDataItemName::Rfc822 => write_section(out, "RFC822", Some(Cow::Borrowed(raw)), None),
        MessageDataItemName::Rfc822Header => {
            write_section(out, "RFC822.HEADER", root.map(|root| Cow::Borrowed(&raw[root.header_range()])), None);
        }
        MessageDataItemName::Rfc822Text => {
            write_section(out, "RFC822.TEXT", root.map(|root| Cow::Borrowed(&raw[root.body_range()])), None);
        }
        // Refused before any response is written; see `supported`.
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use imap_codec::CommandCodec;
    use imap_codec::decode::Decoder;
    use imap_codec::imap_types::command::CommandBody;
    use imap_codec::imap_types::fetch::MacroOrMessageDataItemNames;

    use super::*;
    use crate::mime::parse;

    fn text(build: impl FnOnce(&mut Out)) -> String {
        let mut out = Out::default();
        build(&mut out);
        String::from_utf8(out.bytes).unwrap()
    }

    /// The sections of `FETCH 1 (<items>)`, parsed by the codec the server uses.
    fn sections(items: &str) -> Vec<Option<Section<'static>>> {
        let line = format!("a FETCH 1 ({items})\r\n");
        let (_, command) = CommandCodec::default().decode_static(line.as_bytes()).unwrap();
        let CommandBody::Fetch {
            macro_or_item_names: MacroOrMessageDataItemNames::MessageDataItemNames(names), ..
        } = command.body
        else {
            panic!("not a fetch with items")
        };
        names
            .into_iter()
            .map(|name| match name {
                MessageDataItemName::BodyExt { section, .. } => section,
                other => panic!("not a section: {other:?}"),
            })
            .collect()
    }

    fn fetched(raw: &[u8], item: &str) -> Option<String> {
        let wanted = sections(item).remove(0);
        section(raw, &parse(raw), wanted.as_ref()).map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    }

    const SIMPLE: &[u8] = b"Date: Mon, 5 Jan 2026 10:00:00 +0000\r\nFrom: Ada Lovelace <ada@example.org>\r\n\
To: bob@example.org, \"C, D\" <cd@example.org>\r\nSubject: Hello\r\nMessage-ID: <1@example.org>\r\n\r\nBody text\r\n";

    #[test]
    fn the_envelope_lists_the_headers_in_order() {
        let envelope = text(|out| write_envelope(out, &parse(SIMPLE)));
        assert_eq!(
            envelope,
            concat!(
                r#"("Mon, 5 Jan 2026 10:00:00 +0000" "Hello" "#,
                r#"(("Ada Lovelace" NIL "ada" "example.org")) "#,
                r#"(("Ada Lovelace" NIL "ada" "example.org")) "#,
                r#"(("Ada Lovelace" NIL "ada" "example.org")) "#,
                r#"((NIL NIL "bob" "example.org")("C, D" NIL "cd" "example.org")) "#,
                r#"NIL NIL NIL "<1@example.org>")"#
            )
        );
    }

    #[test]
    fn the_structure_of_a_plain_message() {
        let part = parse(SIMPLE);
        assert_eq!(
            text(|out| write_body_structure(out, &part, false)),
            r#"("TEXT" "PLAIN" ("CHARSET" "US-ASCII") NIL NIL "7BIT" 11 1)"#
        );
        assert_eq!(
            text(|out| write_body_structure(out, &part, true)),
            r#"("TEXT" "PLAIN" ("CHARSET" "US-ASCII") NIL NIL "7BIT" 11 1 NIL NIL NIL NIL)"#
        );
    }

    const MIXED: &[u8] = b"Subject: outer\r\nContent-Type: multipart/mixed; boundary=xx\r\n\r\n--xx\r\n\
Content-Type: text/plain; charset=utf-8\r\n\r\nhello\r\n--xx\r\n\
Content-Type: application/pdf\r\nContent-Transfer-Encoding: base64\r\n\
Content-Disposition: attachment; filename=\"a.pdf\"\r\n\r\nQUJD\r\n--xx\r\n\
Content-Type: message/rfc822\r\n\r\nSubject: inner\r\nContent-Type: multipart/alternative; boundary=yy\r\n\r\n\
--yy\r\n\r\nplain\r\n--yy\r\nContent-Type: text/html\r\n\r\n<p>html</p>\r\n--yy--\r\n\r\n--xx--\r\n";

    #[test]
    fn the_structure_of_a_multipart_message_nests() {
        let structure = text(|out| write_body_structure(out, &parse(MIXED), true));
        assert!(structure.starts_with(r#"(("TEXT" "PLAIN" ("CHARSET" "utf-8") NIL NIL "7BIT" 5 1 NIL NIL NIL NIL)"#));
        assert!(structure.contains(
            r#"("APPLICATION" "PDF" NIL NIL NIL "BASE64" 4 NIL ("ATTACHMENT" ("FILENAME" "a.pdf")) NIL NIL)"#
        ));
        assert!(structure.contains(r#"("MESSAGE" "RFC822" NIL NIL NIL "7BIT" "#));
        assert!(structure.contains(r#"(NIL "inner" NIL NIL NIL NIL NIL NIL NIL NIL)"#), "{structure}");
        assert!(structure.contains(r#""ALTERNATIVE" ("BOUNDARY" "yy") NIL NIL NIL)"#));
        assert!(structure.ends_with(r#" "MIXED" ("BOUNDARY" "xx") NIL NIL NIL)"#));
        assert_eq!(structure.matches('(').count(), structure.matches(')').count());
    }

    #[test]
    fn sections_of_a_plain_message() {
        assert_eq!(fetched(SIMPLE, "BODY[]").unwrap().len(), SIMPLE.len());
        assert_eq!(fetched(SIMPLE, "BODY[TEXT]").unwrap(), "Body text\r\n");
        assert_eq!(fetched(SIMPLE, "BODY[1]").unwrap(), "Body text\r\n");
        assert!(fetched(SIMPLE, "BODY[HEADER]").unwrap().ends_with("<1@example.org>\r\n\r\n"));
        assert_eq!(
            fetched(SIMPLE, "BODY[HEADER.FIELDS (subject FROM)]").unwrap(),
            "From: Ada Lovelace <ada@example.org>\r\nSubject: Hello\r\n\r\n"
        );
        assert_eq!(
            fetched(SIMPLE, "BODY[HEADER.FIELDS.NOT (Date From To Subject)]").unwrap(),
            "Message-ID: <1@example.org>\r\n\r\n"
        );
        assert_eq!(fetched(SIMPLE, "BODY[HEADER.FIELDS (X-Nothing)]").unwrap(), "\r\n");
        assert_eq!(fetched(SIMPLE, "BODY[2]"), None);
    }

    #[test]
    fn sections_of_a_multipart_message() {
        assert_eq!(fetched(MIXED, "BODY[1]").unwrap(), "hello");
        assert_eq!(fetched(MIXED, "BODY[2]").unwrap(), "QUJD");
        assert!(fetched(MIXED, "BODY[2.MIME]").unwrap().starts_with("Content-Type: application/pdf"));
        assert!(fetched(MIXED, "BODY[3]").unwrap().starts_with("Subject: inner"));
        assert_eq!(fetched(MIXED, "BODY[3.HEADER.FIELDS (Subject)]").unwrap(), "Subject: inner\r\n\r\n");
        assert!(fetched(MIXED, "BODY[3.TEXT]").unwrap().starts_with("--yy"));
        assert_eq!(fetched(MIXED, "BODY[3.1]").unwrap(), "plain");
        assert_eq!(fetched(MIXED, "BODY[3.2]").unwrap(), "<p>html</p>");
        assert_eq!(fetched(MIXED, "BODY[4]"), None);
        assert_eq!(fetched(MIXED, "BODY[1.HEADER]"), None, "only an embedded message has a header section");
    }

    #[test]
    fn section_names_are_echoed_in_canonical_form() {
        let names: Vec<_> =
            sections("BODY[] BODY[1.2] BODY[header] BODY[3.text] BODY[1.MIME] BODY.PEEK[header.fields (from To)]")
                .iter()
                .map(|wanted| section_name(wanted.as_ref()))
                .collect();
        assert_eq!(names, ["", "1.2", "HEADER", "3.TEXT", "1.MIME", "HEADER.FIELDS (FROM TO)"]);
    }

    #[test]
    fn internal_dates_use_the_imap_format() {
        assert_eq!(internal_date(UNIX_EPOCH + Duration::from_secs(1_767_607_200)), "05-Jan-2026 10:00:00 +0000");
    }
}
