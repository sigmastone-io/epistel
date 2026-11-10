//! Deciding whether a message matches a SEARCH key.
//!
//! Text is compared as it sits in the message: without decoding encoded words or transfer
//! encodings, and without regard to ASCII case. A search for non-ASCII text therefore finds it
//! only where the message carries it unencoded. Clients search their own copies for the rest.

use chrono::{DateTime, NaiveDate, Utc};
use epistel_core::model::{Keyword, MessageRecord, SystemFlags};
use imap_codec::imap_types::search::SearchKey;
use imap_codec::imap_types::sequence::{SeqOrUid, Sequence, SequenceSet};

use crate::mime::Part;

/// One message being tested.
pub struct Candidate<'a> {
    /// Its sequence number in the session.
    pub seq: u32,
    pub record: &'a MessageRecord,
    /// The raw message and its structure, present when a key [`needs_message`].
    pub message: Option<(&'a [u8], &'a Part)>,
    /// The largest sequence number and the largest UID of the mailbox, which `*` stands for.
    pub last_seq: u32,
    pub last_uid: u32,
}

/// Whether `value` is in the set, with `largest` standing in for `*`.
pub fn in_set(set: &SequenceSet, value: u32, largest: u32) -> bool {
    let number = |bound: &SeqOrUid| match bound {
        SeqOrUid::Value(value) => value.get(),
        SeqOrUid::Asterisk => largest,
    };
    set.0.as_ref().iter().any(|sequence| match sequence {
        Sequence::Single(bound) => number(bound) == value,
        Sequence::Range(a, b) => {
            let (a, b) = (number(a), number(b));
            (a.min(b)..=a.max(b)).contains(&value)
        }
    })
}

/// Whether answering the key takes the message itself, beyond its record.
pub fn needs_message(key: &SearchKey<'_>) -> bool {
    match key {
        SearchKey::And(keys) => keys.as_ref().iter().any(needs_message),
        SearchKey::Not(key) => needs_message(key),
        SearchKey::Or(a, b) => needs_message(a) || needs_message(b),
        SearchKey::Bcc(_)
        | SearchKey::Body(_)
        | SearchKey::Cc(_)
        | SearchKey::From(_)
        | SearchKey::Header(..)
        | SearchKey::SentBefore(_)
        | SearchKey::SentOn(_)
        | SearchKey::SentSince(_)
        | SearchKey::Subject(_)
        | SearchKey::Text(_)
        | SearchKey::To(_) => true,
        _ => false,
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty() || haystack.windows(needle.len()).any(|window| window.eq_ignore_ascii_case(needle))
}

fn header_contains(candidate: &Candidate<'_>, name: &[u8], needle: &[u8]) -> bool {
    let Some((_, root)) = candidate.message else { return false };
    root.fields
        .iter()
        .any(|field| field.name.as_bytes().eq_ignore_ascii_case(name) && contains(field.value.as_bytes(), needle))
}

fn received_on(record: &MessageRecord) -> NaiveDate {
    DateTime::<Utc>::from(record.internal_date).date_naive()
}

/// The date in the `Date` header, in the sender's own time zone as the standard asks.
fn sent_on(candidate: &Candidate<'_>) -> Option<NaiveDate> {
    let (_, root) = candidate.message?;
    let value = &root.field("Date")?.value;
    DateTime::parse_from_rfc2822(value).ok().map(|date| date.date_naive())
}

fn has_keyword(record: &MessageRecord, name: &str) -> bool {
    Keyword::parse(name).is_ok_and(|keyword| record.flags.keywords.contains(&keyword))
}

/// Whether the message matches the key.
pub fn matches(key: &SearchKey<'_>, candidate: &Candidate<'_>) -> bool {
    let record = candidate.record;
    let has = |flag| record.flags.system.contains(flag);
    match key {
        SearchKey::And(keys) => keys.as_ref().iter().all(|key| matches(key, candidate)),
        SearchKey::Or(a, b) => matches(a, candidate) || matches(b, candidate),
        SearchKey::Not(key) => !matches(key, candidate),
        SearchKey::All => true,
        SearchKey::SequenceSet(set) => in_set(set, candidate.seq, candidate.last_seq),
        SearchKey::Uid(set) => in_set(set, record.uid.get(), candidate.last_uid),

        SearchKey::Answered => has(SystemFlags::ANSWERED),
        SearchKey::Unanswered => !has(SystemFlags::ANSWERED),
        SearchKey::Deleted => has(SystemFlags::DELETED),
        SearchKey::Undeleted => !has(SystemFlags::DELETED),
        SearchKey::Draft => has(SystemFlags::DRAFT),
        SearchKey::Undraft => !has(SystemFlags::DRAFT),
        SearchKey::Flagged => has(SystemFlags::FLAGGED),
        SearchKey::Unflagged => !has(SystemFlags::FLAGGED),
        SearchKey::Seen => has(SystemFlags::SEEN),
        SearchKey::Unseen => !has(SystemFlags::SEEN),
        SearchKey::Keyword(name) => has_keyword(record, name.inner()),
        SearchKey::Unkeyword(name) => !has_keyword(record, name.inner()),
        // The \Recent flag is not kept: no message is recent, every message is old.
        SearchKey::Recent | SearchKey::New => false,
        SearchKey::Old => true,

        SearchKey::Larger(size) => record.size > u64::from(*size),
        SearchKey::Smaller(size) => record.size < u64::from(*size),
        SearchKey::Before(date) => received_on(record) < *date.as_ref(),
        SearchKey::On(date) => received_on(record) == *date.as_ref(),
        SearchKey::Since(date) => received_on(record) >= *date.as_ref(),
        SearchKey::SentBefore(date) => sent_on(candidate).is_some_and(|sent| sent < *date.as_ref()),
        SearchKey::SentOn(date) => sent_on(candidate).is_some_and(|sent| sent == *date.as_ref()),
        SearchKey::SentSince(date) => sent_on(candidate).is_some_and(|sent| sent >= *date.as_ref()),

        SearchKey::Bcc(text) => header_contains(candidate, b"Bcc", text.as_ref()),
        SearchKey::Cc(text) => header_contains(candidate, b"Cc", text.as_ref()),
        SearchKey::From(text) => header_contains(candidate, b"From", text.as_ref()),
        SearchKey::To(text) => header_contains(candidate, b"To", text.as_ref()),
        SearchKey::Subject(text) => header_contains(candidate, b"Subject", text.as_ref()),
        SearchKey::Header(name, text) => header_contains(candidate, name.as_ref(), text.as_ref()),
        SearchKey::Body(text) => {
            candidate.message.is_some_and(|(raw, root)| contains(&raw[root.body_range()], text.as_ref()))
        }
        SearchKey::Text(text) => candidate.message.is_some_and(|(raw, _)| contains(raw, text.as_ref())),
        // Keys of extensions this server does not announce.
        #[allow(unreachable_patterns)]
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use epistel_core::model::{Flags, ModSeq, Uid};
    use epistel_core::{BlobId, MailboxId};
    use imap_codec::CommandCodec;
    use imap_codec::decode::Decoder;
    use imap_codec::imap_types::command::CommandBody;

    use super::*;
    use crate::mime::parse;

    const RAW: &[u8] = b"Date: Mon, 5 Jan 2026 23:30:00 -0500\r\nFrom: Ada <ada@example.org>\r\n\
To: bob@example.org\r\nSubject: Quarterly Report\r\n\r\nThe numbers are attached.\r\n";

    fn found(criteria: &str) -> bool {
        let line = format!("a SEARCH {criteria}\r\n");
        let (_, command) = CommandCodec::default().decode_static(line.as_bytes()).unwrap();
        let CommandBody::Search { criteria, .. } = command.body else { panic!("not a search") };
        let mut flags = Flags::from(SystemFlags::SEEN | SystemFlags::FLAGGED);
        flags.keywords.insert(Keyword::parse("$Forwarded").unwrap());
        let record = MessageRecord {
            mailbox: MailboxId::generate(),
            uid: Uid::new(40).unwrap(),
            blob: BlobId::generate(),
            size: RAW.len() as u64,
            // 2026-01-06 08:00:00 UTC
            internal_date: UNIX_EPOCH + Duration::from_secs(1_767_686_400),
            flags,
            modseq: ModSeq(1),
        };
        let root = parse(RAW);
        let candidate = Candidate { seq: 3, record: &record, message: Some((RAW, &root)), last_seq: 3, last_uid: 40 };
        criteria.as_ref().iter().all(|key| matches(key, &candidate))
    }

    #[test]
    fn flags_and_keywords() {
        assert!(found("SEEN FLAGGED UNANSWERED UNDELETED UNDRAFT KEYWORD $Forwarded"));
        assert!(!found("UNSEEN"));
        assert!(!found("KEYWORD $Junk"));
        assert!(found("UNKEYWORD $Junk OLD"));
        assert!(!found("RECENT"));
    }

    #[test]
    fn sets_sizes_and_dates() {
        assert!(found("3") && found("1:*") && found("UID 40") && found("UID 30:*") && found("UID *"));
        assert!(!found("1:2") && !found("UID 1:39"));
        assert!(found("LARGER 10 SMALLER 100000") && !found("SMALLER 10"));
        assert!(found("ON 6-Jan-2026 SINCE 6-Jan-2026 BEFORE 7-Jan-2026"));
        assert!(!found("BEFORE 6-Jan-2026") && !found("SINCE 7-Jan-2026"));
        assert!(found("SENTON 5-Jan-2026"), "the sender's own date counts, not the date in UTC");
        assert!(found("SENTSINCE 1-Jan-2026 SENTBEFORE 6-Jan-2026"));
    }

    #[test]
    fn text_is_matched_without_regard_to_case() {
        assert!(found("FROM ada SUBJECT \"quarterly report\" TO BOB@EXAMPLE"));
        assert!(found("HEADER Subject report") && found("HEADER subject \"\""));
        assert!(!found("HEADER X-Missing \"\"") && !found("CC bob"));
        assert!(found("BODY attached") && !found("BODY quarterly"), "the body does not include the headers");
        assert!(found("TEXT quarterly") && found("TEXT attached"));
    }

    #[test]
    fn keys_combine() {
        assert!(found("OR UNSEEN FLAGGED"));
        assert!(found("NOT UNSEEN"));
        assert!(!found("OR UNSEEN DELETED"));
        assert!(found("NOT (UNSEEN DELETED)"));
        assert!(found("(SEEN FROM ada) NOT DRAFT"));
    }
}
