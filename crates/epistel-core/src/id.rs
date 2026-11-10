//! Identifiers. Each is a random UUID wrapped in its own type, so that an account id cannot be
//! passed where a mailbox id is expected.

use std::fmt;
use std::str::FromStr;

use uuid::Uuid;

/// The text was not a valid identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid identifier")]
pub struct InvalidId;

macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(Uuid);

        impl $name {
            /// A fresh random identifier.
            pub fn generate() -> Self {
                Self(Uuid::new_v4())
            }

            /// Wraps an existing UUID, for a backend reading one back from storage.
            pub const fn from_uuid(value: Uuid) -> Self {
                Self(value)
            }

            /// The underlying UUID.
            pub const fn as_uuid(&self) -> Uuid {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.as_simple().fmt(f)
            }
        }

        impl FromStr for $name {
            type Err = InvalidId;

            fn from_str(text: &str) -> Result<Self, Self::Err> {
                Uuid::try_parse(text).map(Self).map_err(|_| InvalidId)
            }
        }
    };
}

define_id!(
    /// Identifies an account.
    AccountId
);
define_id!(
    /// Identifies one app password of an account.
    CredentialId
);
define_id!(
    /// Identifies a mailbox. Stays the same when the mailbox is renamed.
    MailboxId
);
define_id!(
    /// Identifies a stored raw message.
    BlobId
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_form_round_trips() {
        let id = MailboxId::generate();
        assert_eq!(id.to_string().parse::<MailboxId>(), Ok(id));
        assert_eq!("not an id".parse::<MailboxId>(), Err(InvalidId));
    }
}
