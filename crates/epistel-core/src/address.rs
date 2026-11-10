//! Mail addresses and the patterns an account can own.

use std::fmt;
use std::str::FromStr;

/// The text is not an address Epistel accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid mail address")]
pub struct InvalidAddress;

/// A mail address, `local@domain`, in normalized form.
///
/// Normalization lowercases the whole address. The standards allow a case-sensitive local part,
/// but no deployed system relies on it, and treating `Alice@` and `alice@` as two mailboxes would
/// only lose mail.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EmailAddress {
    value: String,
    at: usize,
}

impl EmailAddress {
    /// Parses and normalizes an address.
    pub fn parse(input: &str) -> Result<Self, InvalidAddress> {
        let value = input.to_lowercase();
        if value.len() > 254 || value.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(InvalidAddress);
        }
        let at = value.rfind('@').ok_or(InvalidAddress)?;
        let local = &value[..at];
        if local.is_empty() || local.len() > 64 {
            return Err(InvalidAddress);
        }
        validate_domain(&value[at + 1..])?;
        Ok(Self { value, at })
    }

    /// The part before the `@`.
    pub fn local_part(&self) -> &str {
        &self.value[..self.at]
    }

    /// The part after the `@`.
    pub fn domain(&self) -> &str {
        &self.value[self.at + 1..]
    }

    /// The whole address.
    pub fn as_str(&self) -> &str {
        &self.value
    }
}

impl fmt::Display for EmailAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.value)
    }
}

impl FromStr for EmailAddress {
    type Err = InvalidAddress;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

fn validate_domain(domain: &str) -> Result<(), InvalidAddress> {
    let label_ok = |label: &str| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.chars().all(|c| c.is_alphanumeric() || c == '-')
    };
    if domain.split('.').all(label_ok) { Ok(()) } else { Err(InvalidAddress) }
}

/// What an account can own: one address, or every address of a domain not owned otherwise.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AddressPattern {
    /// Exactly this address.
    Exact(EmailAddress),
    /// Catch-all for the domain, written `*@domain`.
    Domain(String),
}

impl AddressPattern {
    /// Parses `local@domain` or `*@domain`.
    pub fn parse(input: &str) -> Result<Self, InvalidAddress> {
        match input.strip_prefix("*@") {
            Some(domain) => {
                let domain = domain.to_lowercase();
                validate_domain(&domain)?;
                Ok(Self::Domain(domain))
            }
            None => EmailAddress::parse(input).map(Self::Exact),
        }
    }

    /// The catch-all pattern that would cover `address`.
    pub fn catch_all_for(address: &EmailAddress) -> Self {
        Self::Domain(address.domain().to_owned())
    }
}

impl From<EmailAddress> for AddressPattern {
    fn from(address: EmailAddress) -> Self {
        Self::Exact(address)
    }
}

impl fmt::Display for AddressPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exact(address) => address.fmt(f),
            Self::Domain(domain) => write!(f, "*@{domain}"),
        }
    }
}

impl FromStr for AddressPattern {
    type Err = InvalidAddress;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_lowercased_and_split() {
        let address = EmailAddress::parse("Alice.Smith@Example.ORG").unwrap();
        assert_eq!(address.as_str(), "alice.smith@example.org");
        assert_eq!(address.local_part(), "alice.smith");
        assert_eq!(address.domain(), "example.org");
    }

    #[test]
    fn malformed_addresses_are_refused() {
        for bad in ["", "alice", "@example.org", "alice@", "al ice@example.org", "alice@-example.org", "alice@a..b"] {
            assert_eq!(EmailAddress::parse(bad), Err(InvalidAddress), "{bad:?}");
        }
    }

    #[test]
    fn patterns_parse_both_forms() {
        assert_eq!(AddressPattern::parse("*@Example.org"), Ok(AddressPattern::Domain("example.org".into())));
        let exact = AddressPattern::parse("bob@example.org").unwrap();
        assert_eq!(exact.to_string(), "bob@example.org");
        let address = EmailAddress::parse("bob@example.org").unwrap();
        assert_eq!(AddressPattern::catch_all_for(&address).to_string(), "*@example.org");
    }
}
