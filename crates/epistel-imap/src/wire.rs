// SPDX-FileCopyrightText: 2026 Sigmastone OÜ
// SPDX-License-Identifier: Apache-2.0

//! Writing IMAP responses.

/// A response under construction.
#[derive(Default)]
pub struct Out {
    pub bytes: Vec<u8>,
}

impl Out {
    /// Text that is already valid on the wire: keywords, atoms, punctuation.
    pub fn raw(&mut self, text: &str) -> &mut Self {
        self.bytes.extend_from_slice(text.as_bytes());
        self
    }

    pub fn sp(&mut self) -> &mut Self {
        self.bytes.push(b' ');
        self
    }

    pub fn crlf(&mut self) -> &mut Self {
        self.bytes.extend_from_slice(b"\r\n");
        self
    }

    pub fn number(&mut self, value: u64) -> &mut Self {
        self.raw(&value.to_string())
    }

    /// Arbitrary bytes with their length in front. The only form that can carry anything.
    pub fn literal(&mut self, value: &[u8]) -> &mut Self {
        self.raw(&format!("{{{}}}\r\n", value.len()));
        self.bytes.extend_from_slice(value);
        self
    }

    /// A string: in quotes when that is allowed for its content, as a literal otherwise.
    pub fn string(&mut self, value: &[u8]) -> &mut Self {
        let quotable = value.len() <= 512
            && value.iter().all(|byte| matches!(byte, 0x01..=0x7f) && !matches!(byte, b'\r' | b'\n'));
        if !quotable {
            return self.literal(value);
        }
        self.bytes.push(b'"');
        for byte in value {
            if matches!(byte, b'"' | b'\\') {
                self.bytes.push(b'\\');
            }
            self.bytes.push(*byte);
        }
        self.bytes.push(b'"');
        self
    }

    /// A string, or `NIL` for none.
    pub fn nstring(&mut self, value: Option<&str>) -> &mut Self {
        match value {
            Some(value) => self.string(value.as_bytes()),
            None => self.raw("NIL"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The writers return the writer for chaining; here the result is the bytes.
    fn ignore(_: &mut Out) {}

    fn written(build: impl FnOnce(&mut Out)) -> String {
        let mut out = Out::default();
        build(&mut out);
        String::from_utf8(out.bytes).unwrap()
    }

    #[test]
    fn strings_are_quoted_when_possible_and_literals_otherwise() {
        assert_eq!(written(|out| ignore(out.string(b"plain text"))), "\"plain text\"");
        assert_eq!(written(|out| ignore(out.string(br#"a "b" \c"#))), r#""a \"b\" \\c""#);
        assert_eq!(written(|out| ignore(out.string(b"two\r\nlines"))), "{10}\r\ntwo\r\nlines");
        assert_eq!(written(|out| ignore(out.string("\u{e5}".as_bytes()))), "{2}\r\n\u{e5}");
        assert_eq!(written(|out| ignore(out.string(b""))), "\"\"");
        assert_eq!(written(|out| ignore(out.nstring(None).sp().nstring(Some("x")))), "NIL \"x\"");
    }
}
