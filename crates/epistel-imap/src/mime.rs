//! The MIME structure of a message, as byte ranges into the raw message.
//!
//! IMAP hands out pieces of the message exactly as they were received and reports sizes in
//! octets of the undecoded text, so nothing here decodes or copies content. Parsing never fails:
//! whatever arrives is described as well as it can be, the way mail clients expect of a server.

use std::ops::Range;

/// One header field of a header block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    /// The name as written, without the colon.
    pub name: String,
    /// The value with folding removed and surrounding space trimmed. Not decoded.
    pub value: String,
    /// The whole field in the raw message, name to line end, continuation lines included.
    pub range: Range<usize>,
}

/// A message, or one part of a multipart message.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Part {
    /// Where the header block begins.
    pub start: usize,
    /// Where the body begins, after the blank line.
    pub body: usize,
    /// Where the body ends.
    pub end: usize,
    pub fields: Vec<Field>,
    /// Media type, upper case, as IMAP reports it.
    pub media_type: String,
    pub subtype: String,
    /// Content-Type parameters, names upper case.
    pub parameters: Vec<(String, String)>,
    pub id: Option<String>,
    pub description: Option<String>,
    /// Content-Transfer-Encoding, upper case; `7BIT` when absent.
    pub encoding: String,
    /// Content-Disposition: kind (upper case) and parameters.
    pub disposition: Option<(String, Vec<(String, String)>)>,
    /// Lines of the body.
    pub lines: usize,
    /// The parts of a multipart body.
    pub children: Vec<Part>,
    /// The message inside a `message/rfc822` part.
    pub message: Option<Box<Part>>,
}

impl Part {
    /// The first field of that name, any case.
    pub fn field(&self, name: &str) -> Option<&Field> {
        self.fields.iter().find(|field| field.name.eq_ignore_ascii_case(name))
    }

    /// The values of every field of that name.
    pub fn values<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> {
        self.fields.iter().filter(move |field| field.name.eq_ignore_ascii_case(name)).map(|field| field.value.as_str())
    }

    pub fn header_range(&self) -> Range<usize> {
        self.start..self.body
    }

    pub fn body_range(&self) -> Range<usize> {
        self.body..self.end
    }

    pub fn is_multipart(&self) -> bool {
        self.media_type == "MULTIPART"
    }
}

/// Describes a whole raw message.
pub fn parse(raw: &[u8]) -> Part {
    parse_part(raw, 0..raw.len(), ("TEXT", "PLAIN"), 0)
}

/// How deep parts may nest before the rest is treated as opaque text. Real mail stays far below.
const DEPTH_LIMIT: usize = 32;

/// The end of the line starting at `at`, past its line break, and the end of its content.
fn line_end(raw: &[u8], at: usize, limit: usize) -> (usize, usize) {
    match raw[at..limit].iter().position(|byte| *byte == b'\n') {
        Some(offset) => {
            let newline = at + offset;
            let content_end = if newline > at && raw[newline - 1] == b'\r' { newline - 1 } else { newline };
            (newline + 1, content_end)
        }
        None => (limit, limit),
    }
}

fn parse_fields(raw: &[u8], range: Range<usize>) -> (Vec<Field>, usize) {
    let mut fields = Vec::new();
    let mut at = range.start;
    while at < range.end {
        let (next, content_end) = line_end(raw, at, range.end);
        if content_end == at {
            // The blank line: the body starts after it.
            return (fields, next);
        }
        let mut field_end = next;
        while field_end < range.end && matches!(raw[field_end], b' ' | b'\t') {
            field_end = line_end(raw, field_end, range.end).0;
        }
        let text = String::from_utf8_lossy(&raw[at..field_end]);
        if let Some((name, value)) = text.split_once(':') {
            let unfolded: String = value.split(['\r', '\n']).collect();
            fields.push(Field {
                name: name.trim().to_owned(),
                value: unfolded.trim().to_owned(),
                range: at..field_end,
            });
        }
        at = field_end;
    }
    (fields, range.end)
}

/// Splits `text/plain; charset=utf-8; name="a b"` into the leading token and its parameters.
/// Parameter names come back upper case.
pub fn parse_parameters(value: &str) -> (String, Vec<(String, String)>) {
    let mut pieces = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut escaped = false;
    let mut comment = 0usize;
    for c in value.chars() {
        if escaped {
            current.push(c);
            escaped = false;
        } else if quoted {
            match c {
                '\\' => escaped = true,
                '"' => quoted = false,
                _ => current.push(c),
            }
        } else if comment > 0 {
            match c {
                '(' => comment += 1,
                ')' => comment -= 1,
                _ => {}
            }
        } else {
            match c {
                '"' => quoted = true,
                '(' => comment += 1,
                ';' => pieces.push(std::mem::take(&mut current)),
                _ => current.push(c),
            }
        }
    }
    pieces.push(current);
    let mut pieces = pieces.into_iter();
    let head = pieces.next().unwrap_or_default().trim().to_owned();
    let parameters = pieces
        .filter_map(|piece| {
            let (name, value) = piece.split_once('=')?;
            let name = name.trim();
            (!name.is_empty()).then(|| (name.to_ascii_uppercase(), value.trim().to_owned()))
        })
        .collect();
    (head, parameters)
}

fn count_lines(body: &[u8]) -> usize {
    let breaks = body.iter().filter(|byte| **byte == b'\n').count();
    if body.last().is_some_and(|byte| *byte != b'\n') { breaks + 1 } else { breaks }
}

fn parse_part(raw: &[u8], range: Range<usize>, default_type: (&str, &str), depth: usize) -> Part {
    let (fields, body) = parse_fields(raw, range.clone());
    let mut part = Part { start: range.start, body, end: range.end, fields, ..Part::default() };

    let content_type = part.field("Content-Type").map(|field| parse_parameters(&field.value));
    let named = content_type.as_ref().and_then(|(head, _)| head.split_once('/'));
    match (named, content_type.as_ref()) {
        (Some((media_type, subtype)), Some((_, parameters))) if !media_type.trim().is_empty() => {
            part.media_type = media_type.trim().to_ascii_uppercase();
            part.subtype = subtype.trim().to_ascii_uppercase();
            part.parameters = parameters.clone();
        }
        _ => {
            part.media_type = default_type.0.to_owned();
            part.subtype = default_type.1.to_owned();
        }
    }
    if part.media_type == "TEXT" && !part.parameters.iter().any(|(name, _)| name == "CHARSET") {
        part.parameters.push(("CHARSET".to_owned(), "US-ASCII".to_owned()));
    }
    part.id = part.field("Content-ID").map(|field| field.value.clone());
    part.description = part.field("Content-Description").map(|field| field.value.clone());
    part.encoding = part
        .field("Content-Transfer-Encoding")
        .map(|field| field.value.trim().to_ascii_uppercase())
        .filter(|encoding| !encoding.is_empty())
        .unwrap_or_else(|| "7BIT".to_owned());
    part.disposition = part.field("Content-Disposition").map(|field| {
        let (kind, parameters) = parse_parameters(&field.value);
        (kind.to_ascii_uppercase(), parameters)
    });
    part.lines = count_lines(&raw[part.body_range()]);

    if depth >= DEPTH_LIMIT {
        return part;
    }
    if part.is_multipart() {
        let boundary = part.parameters.iter().find(|(name, _)| name == "BOUNDARY").map(|(_, value)| value.clone());
        if let Some(boundary) = boundary.filter(|boundary| !boundary.is_empty()) {
            let inner_default = if part.subtype == "DIGEST" { ("MESSAGE", "RFC822") } else { ("TEXT", "PLAIN") };
            for child in split_multipart(raw, part.body_range(), boundary.as_bytes()) {
                part.children.push(parse_part(raw, child, inner_default, depth + 1));
            }
        }
    } else if part.media_type == "MESSAGE" && part.subtype == "RFC822" {
        // An encoded embedded message cannot be described without decoding it; that is not allowed
        // by the standard and is left opaque here.
        if matches!(part.encoding.as_str(), "7BIT" | "8BIT" | "BINARY") {
            part.message = Some(Box::new(parse_part(raw, part.body_range(), ("TEXT", "PLAIN"), depth + 1)));
        }
    }
    part
}

/// The ranges of the parts between the boundary lines of a multipart body.
fn split_multipart(raw: &[u8], body: Range<usize>, boundary: &[u8]) -> Vec<Range<usize>> {
    let mut parts = Vec::new();
    let mut open: Option<usize> = None;
    let mut at = body.start;
    while at < body.end {
        let (next, content_end) = line_end(raw, at, body.end);
        let line = &raw[at..content_end];
        let after_dashes = line.strip_prefix(b"--").and_then(|rest| rest.strip_prefix(boundary));
        if let Some(tail) = after_dashes {
            let closing = tail.starts_with(b"--");
            let padding = if closing { &tail[2..] } else { tail };
            if padding.iter().all(|byte| matches!(byte, b' ' | b'\t')) {
                if let Some(start) = open.take() {
                    // The line break before a boundary line belongs to the boundary.
                    let mut end = at;
                    if end > start && raw[end - 1] == b'\n' {
                        end -= 1;
                        if end > start && raw[end - 1] == b'\r' {
                            end -= 1;
                        }
                    }
                    parts.push(start..end.max(start));
                }
                if closing {
                    return parts;
                }
                open = Some(next);
            }
        }
        at = next;
    }
    // A missing closing boundary: the last part runs to the end.
    if let Some(start) = open {
        parts.push(start..body.end);
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(raw: &[u8], range: Range<usize>) -> &str {
        std::str::from_utf8(&raw[range]).unwrap()
    }

    #[test]
    fn a_plain_message_has_headers_a_body_and_defaults() {
        let raw = b"From: a@example.org\r\nSubject: a folded\r\n subject\r\n\r\nline one\r\nline two\r\n";
        let part = parse(raw);
        assert_eq!(text(raw, part.header_range()), "From: a@example.org\r\nSubject: a folded\r\n subject\r\n\r\n");
        assert_eq!(text(raw, part.body_range()), "line one\r\nline two\r\n");
        assert_eq!(part.field("subject").unwrap().value, "a folded subject");
        assert_eq!((part.media_type.as_str(), part.subtype.as_str()), ("TEXT", "PLAIN"));
        assert_eq!(part.parameters, [("CHARSET".to_owned(), "US-ASCII".to_owned())]);
        assert_eq!(part.encoding, "7BIT");
        assert_eq!(part.lines, 2);
    }

    #[test]
    fn a_message_without_a_body_or_with_bare_newlines_is_still_described() {
        let headers_only = parse(b"Subject: x\r\n");
        assert_eq!(headers_only.body_range(), 12..12);
        let bare = b"Subject: x\n\nbody";
        let part = parse(bare);
        assert_eq!(text(bare, part.body_range()), "body");
        assert_eq!(part.lines, 1);
        assert_eq!(parse(b"").fields, []);
    }

    #[test]
    fn parameters_handle_quotes_comments_and_case() {
        let (head, parameters) = parse_parameters(r#"Text/HTML; charset="utf-8" (a comment); Name="a; \"b\".txt""#);
        assert_eq!(head, "Text/HTML");
        assert_eq!(parameters, [("CHARSET".into(), "utf-8".into()), ("NAME".into(), "a; \"b\".txt".into())]);
    }

    const MULTIPART: &[u8] = b"Content-Type: multipart/mixed; boundary=\"xx\"\r\n\r\npreamble\r\n--xx\r\n\
Content-Type: text/plain; charset=utf-8\r\n\r\nhello\r\n--xx\r\n\
Content-Type: application/pdf; name=\"a.pdf\"\r\nContent-Transfer-Encoding: base64\r\n\
Content-Disposition: attachment; filename=\"a.pdf\"\r\nContent-ID: <id1>\r\n\r\nQUJD\r\n--xx--\r\nepilogue\r\n";

    #[test]
    fn a_multipart_body_is_split_at_its_boundaries() {
        let part = parse(MULTIPART);
        assert_eq!(part.subtype, "MIXED");
        assert_eq!(part.children.len(), 2);
        let (first, second) = (&part.children[0], &part.children[1]);
        assert_eq!(text(MULTIPART, first.body_range()), "hello");
        assert_eq!(first.parameters, [("CHARSET".to_owned(), "utf-8".to_owned())]);
        assert_eq!(text(MULTIPART, second.body_range()), "QUJD");
        assert_eq!(second.encoding, "BASE64");
        assert_eq!(second.id.as_deref(), Some("<id1>"));
        assert_eq!(second.disposition, Some(("ATTACHMENT".into(), vec![("FILENAME".into(), "a.pdf".into())])));
        assert!(text(MULTIPART, second.header_range()).starts_with("Content-Type: application/pdf"));
    }

    #[test]
    fn a_missing_closing_boundary_and_a_missing_boundary_parameter_are_survived() {
        let unclosed = b"Content-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\n\r\none\r\n--b\r\n\r\ntwo";
        let part = parse(unclosed);
        assert_eq!(part.children.len(), 2);
        assert_eq!(text(unclosed, part.children[1].body_range()), "two");
        assert!(parse(b"Content-Type: multipart/mixed\r\n\r\n--b\r\n\r\none\r\n--b--\r\n").children.is_empty());
    }

    #[test]
    fn an_embedded_message_is_described_too() {
        let raw = b"Content-Type: message/rfc822\r\n\r\nSubject: inner\r\nContent-Type: text/html\r\n\r\n<p>hi</p>\r\n";
        let part = parse(raw);
        let inner = part.message.as_ref().unwrap();
        assert_eq!(inner.field("Subject").unwrap().value, "inner");
        assert_eq!(inner.subtype, "HTML");
        assert_eq!(text(raw, inner.body_range()), "<p>hi</p>\r\n");
        assert_eq!(part.lines, 4);
    }

    #[test]
    fn nesting_without_end_stops_at_the_limit() {
        let mut raw = Vec::new();
        for _ in 0..200 {
            raw.extend_from_slice(b"Content-Type: message/rfc822\r\n\r\n");
        }
        let mut depth = 0;
        let mut part = &parse(&raw);
        while let Some(inner) = &part.message {
            part = inner;
            depth += 1;
        }
        assert_eq!(depth, DEPTH_LIMIT);
    }
}
