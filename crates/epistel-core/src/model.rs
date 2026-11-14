// SPDX-FileCopyrightText: 2026 Sigmastone OÜ
// SPDX-License-Identifier: Apache-2.0

//! Accounts, mailboxes and message records.

use std::collections::BTreeSet;
use std::fmt;
use std::num::NonZeroU32;
use std::ops::BitOr;
use std::time::{Duration, SystemTime};

use crate::address::EmailAddress;
use crate::id::{AccountId, BlobId, CredentialId, MailboxId};

// ---------------------------------------------------------------------------------------------
// Accounts and credentials
// ---------------------------------------------------------------------------------------------

/// Whether an account may log in and receive mail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccountState {
    /// Normal operation.
    Active,
    /// Logins are refused. Mail is still delivered.
    Suspended,
}

/// A person or function that owns mailboxes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    pub id: AccountId,
    /// The login name. Always one of the addresses the account owns.
    pub primary_address: EmailAddress,
    pub display_name: String,
    pub state: AccountState,
    pub created: SystemTime,
}

/// What is needed to create an account.
#[derive(Clone, Debug)]
pub struct NewAccount {
    pub primary_address: EmailAddress,
    pub display_name: String,
}

/// One app password of an account. Only the hash is ever stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credential {
    pub id: CredentialId,
    /// What the user called it, typically the device.
    pub label: String,
    /// Password hash in PHC string format.
    pub secret_hash: String,
    pub created: SystemTime,
    pub last_used: Option<SystemTime>,
}

/// What is needed to add a credential.
#[derive(Clone, Debug)]
pub struct NewCredential {
    pub label: String,
    pub secret_hash: String,
}

// ---------------------------------------------------------------------------------------------
// Mailboxes
// ---------------------------------------------------------------------------------------------

/// The text is not a usable mailbox name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid mailbox name")]
pub struct InvalidMailboxName;

/// A mailbox name. Levels of the hierarchy are separated by `/`.
///
/// `INBOX` is case-insensitive in IMAP and is stored in upper case; every other name keeps its
/// case and is compared exactly.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MailboxName(String);

impl MailboxName {
    /// The hierarchy separator.
    pub const DELIMITER: char = '/';

    /// Validates a name.
    pub fn parse(input: &str) -> Result<Self, InvalidMailboxName> {
        let segment_ok = |segment: &str| !segment.is_empty() && !segment.chars().any(char::is_control);
        if input.len() > 255 || !input.split(Self::DELIMITER).all(segment_ok) {
            return Err(InvalidMailboxName);
        }
        let mut segments = input.split(Self::DELIMITER);
        let first = segments.next().unwrap_or_default();
        if first.eq_ignore_ascii_case("INBOX") {
            let rest = &input[first.len()..];
            return Ok(Self(format!("INBOX{rest}")));
        }
        Ok(Self(input.to_owned()))
    }

    /// The inbox.
    pub fn inbox() -> Self {
        Self("INBOX".to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MailboxName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The job a mailbox has for its account, beyond being a folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MailboxRole {
    Inbox,
    Drafts,
    Sent,
    Junk,
    Trash,
    Archive,
    /// Holds messages that failed the virus check. Not shown to mail clients.
    Quarantine,
}

/// Changes whenever the UIDs of a mailbox stop being comparable with earlier ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UidValidity(pub NonZeroU32);

/// A folder of messages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mailbox {
    pub id: MailboxId,
    pub account: AccountId,
    pub name: MailboxName,
    pub role: Option<MailboxRole>,
    pub subscribed: bool,
    pub uid_validity: UidValidity,
    /// The UID the next appended message will get.
    pub next_uid: Uid,
    /// The modification sequence of the latest change to any message of the mailbox.
    pub highest_modseq: ModSeq,
}

// ---------------------------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------------------------

/// The identifier of a message inside its mailbox. Grows with every append and is never reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Uid(NonZeroU32);

impl Uid {
    /// The UID of the first message ever appended to a mailbox.
    pub const FIRST: Self = Self(NonZeroU32::MIN);
    /// The largest UID.
    pub const MAX: Self = Self(NonZeroU32::MAX);

    /// `None` for zero, which is not a UID.
    pub fn new(value: u32) -> Option<Self> {
        NonZeroU32::new(value).map(Self)
    }

    pub fn get(self) -> u32 {
        self.0.get()
    }

    /// The UID after this one, or `None` at the end of the range.
    pub fn next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

impl fmt::Display for Uid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// An inclusive range of UIDs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UidRange {
    pub first: Uid,
    pub last: Uid,
}

impl UidRange {
    /// Every UID.
    pub const ALL: Self = Self { first: Uid::FIRST, last: Uid::MAX };

    /// A range holding one UID.
    pub fn single(uid: Uid) -> Self {
        Self { first: uid, last: uid }
    }

    pub fn contains(&self, uid: Uid) -> bool {
        self.first <= uid && uid <= self.last
    }
}

/// A per-mailbox counter that grows with every change to a message.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ModSeq(pub u64);

impl ModSeq {
    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// The flags IMAP defines itself.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct SystemFlags(u8);

impl SystemFlags {
    pub const SEEN: Self = Self(1);
    pub const ANSWERED: Self = Self(1 << 1);
    pub const FLAGGED: Self = Self(1 << 2);
    pub const DELETED: Self = Self(1 << 3);
    pub const DRAFT: Self = Self(1 << 4);

    const NAMES: [(Self, &'static str); 5] = [
        (Self::SEEN, "\\Seen"),
        (Self::ANSWERED, "\\Answered"),
        (Self::FLAGGED, "\\Flagged"),
        (Self::DELETED, "\\Deleted"),
        (Self::DRAFT, "\\Draft"),
    ];

    pub const fn empty() -> Self {
        Self(0)
    }

    /// The flags as a number, for a backend to store.
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// The flags a backend stored. Bits that mean nothing are dropped.
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits & 0b1_1111)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn difference(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    /// The IMAP names of the flags that are set.
    pub fn names(self) -> impl Iterator<Item = &'static str> {
        Self::NAMES.into_iter().filter(move |(flag, _)| self.contains(*flag)).map(|(_, name)| name)
    }
}

impl BitOr for SystemFlags {
    type Output = Self;

    fn bitor(self, other: Self) -> Self {
        self.union(other)
    }
}

impl fmt::Debug for SystemFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.names()).finish()
    }
}

/// The text is not a usable keyword.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid keyword")]
pub struct InvalidKeyword;

/// A flag defined by the user or the client, such as `$Forwarded`. An IMAP atom.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Keyword(String);

impl Keyword {
    /// Validates a keyword.
    pub fn parse(input: &str) -> Result<Self, InvalidKeyword> {
        let char_ok = |c: char| c.is_ascii_graphic() && !"(){%*\"\\]".contains(c);
        if input.is_empty() || input.len() > 64 || !input.chars().all(char_ok) {
            return Err(InvalidKeyword);
        }
        Ok(Self(input.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// All flags of one message.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Flags {
    pub system: SystemFlags,
    pub keywords: BTreeSet<Keyword>,
}

impl Flags {
    /// Applies `change` and says whether anything is different afterwards.
    pub fn apply(&mut self, change: &FlagChange) -> bool {
        let before = self.clone();
        match change {
            FlagChange::Replace(flags) => *self = flags.clone(),
            FlagChange::Add(flags) => {
                self.system = self.system.union(flags.system);
                self.keywords.extend(flags.keywords.iter().cloned());
            }
            FlagChange::Remove(flags) => {
                self.system = self.system.difference(flags.system);
                self.keywords.retain(|keyword| !flags.keywords.contains(keyword));
            }
        }
        *self != before
    }
}

impl From<SystemFlags> for Flags {
    fn from(system: SystemFlags) -> Self {
        Self { system, keywords: BTreeSet::new() }
    }
}

/// How to change the flags of a message.
#[derive(Clone, Debug)]
pub enum FlagChange {
    Replace(Flags),
    Add(Flags),
    Remove(Flags),
}

/// Makes an append happen at most once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Once {
    /// Names the delivery. A second append with the same key is refused, in whatever mailbox.
    pub key: String,
    /// How long the key is remembered. Longer than the source can keep offering the message.
    pub remember_for: Duration,
}

/// What is needed to add a message to a mailbox.
#[derive(Clone, Debug)]
pub struct NewMessage {
    /// The stored raw message. Several records may point at the same blob.
    pub blob: BlobId,
    /// Size of the raw message in bytes.
    pub size: u64,
    /// When the message arrived, as IMAP reports it.
    pub internal_date: SystemTime,
    pub flags: Flags,
    /// Set when the same message may be offered more than once, as mail from an inbound source
    /// is. Leave empty for a message that a client appends or copies.
    pub once: Option<Once>,
}

/// A message as it sits in one mailbox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageRecord {
    pub mailbox: MailboxId,
    pub uid: Uid,
    pub blob: BlobId,
    pub size: u64,
    pub internal_date: SystemTime,
    pub flags: Flags,
    /// The modification sequence of the latest change to this message.
    pub modseq: ModSeq,
}

/// What removing messages left behind.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Expunged {
    /// The UIDs that existed and are gone now, ascending.
    pub uids: Vec<Uid>,
    /// Blobs no record points at any more. The caller deletes them from the blob store.
    pub released_blobs: Vec<BlobId>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inbox_is_normalized_and_bad_names_are_refused() {
        assert_eq!(MailboxName::parse("inbox").unwrap().as_str(), "INBOX");
        assert_eq!(MailboxName::parse("Inbox/Lists").unwrap().as_str(), "INBOX/Lists");
        assert_eq!(MailboxName::parse("Inboxes").unwrap().as_str(), "Inboxes");
        for bad in ["", "/a", "a/", "a//b", "a\nb"] {
            assert_eq!(MailboxName::parse(bad), Err(InvalidMailboxName), "{bad:?}");
        }
    }

    #[test]
    fn flag_changes_report_whether_anything_changed() {
        let mut flags = Flags::default();
        assert!(flags.apply(&FlagChange::Add(SystemFlags::SEEN.into())));
        assert!(!flags.apply(&FlagChange::Add(SystemFlags::SEEN.into())));
        assert!(!flags.apply(&FlagChange::Remove(SystemFlags::FLAGGED.into())));
        assert!(flags.apply(&FlagChange::Replace((SystemFlags::SEEN | SystemFlags::DRAFT).into())));
        assert_eq!(flags.system.names().collect::<Vec<_>>(), ["\\Seen", "\\Draft"]);
    }

    #[test]
    fn keywords_are_atoms() {
        assert!(Keyword::parse("$Forwarded").is_ok());
        for bad in ["", "two words", "\\Seen", "a(b"] {
            assert_eq!(Keyword::parse(bad), Err(InvalidKeyword), "{bad:?}");
        }
    }
}
