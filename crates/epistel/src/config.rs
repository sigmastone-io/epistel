//! The configuration file.
//!
//! One TOML file. Each backend section carries a `backend` key that picks the implementation; the
//! other keys of the section belong to that implementation. Unknown keys are errors, so a typo
//! does not silently become a default.

use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde::de::{self, Deserializer, Visitor};

/// Why the configuration could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Read { path: PathBuf, source: std::io::Error },
    #[error("{path}: {source}")]
    Parse {
        path: PathBuf,
        source: Box<toml::de::Error>,
    },
    #[error("{0}")]
    Invalid(String),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub server: Server,
    pub tls: Tls,
    #[serde(default)]
    pub imap: Imap,
    #[serde(default)]
    pub submission: Submission,
    pub metadata: MetadataBackend,
    pub blobs: BlobBackend,
    pub inbound: InboundBackend,
    pub outbound: OutboundBackend,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    /// The name clients connect to and the certificate is issued for.
    pub hostname: String,
}

/// Certificate and key, PEM files. Both listeners are TLS only, so there is no way to leave this out.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tls {
    pub certificate: PathBuf,
    pub private_key: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Imap {
    pub listen: SocketAddr,
}

impl Default for Imap {
    fn default() -> Self {
        Self {
            listen: "[::]:993".parse().expect("valid address"),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Submission {
    pub listen: SocketAddr,
    /// Largest message a client may submit. The outbound relay's own limit applies as well.
    pub max_message_size: ByteSize,
}

impl Default for Submission {
    fn default() -> Self {
        Self {
            listen: "[::]:465".parse().expect("valid address"),
            max_message_size: ByteSize(40 * 1024 * 1024),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "backend", rename_all = "kebab-case", deny_unknown_fields)]
pub enum MetadataBackend {
    /// Kept in the process and lost when it ends. For trying things out.
    Memory {},
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "backend", rename_all = "kebab-case", deny_unknown_fields)]
pub enum BlobBackend {
    /// Kept in the process and lost when it ends. For trying things out.
    Memory {},
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "backend", rename_all = "kebab-case", deny_unknown_fields)]
pub enum InboundBackend {
    /// Receives nothing.
    Memory {},
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "backend", rename_all = "kebab-case", deny_unknown_fields)]
pub enum OutboundBackend {
    /// Accepts mail and sends it nowhere.
    Memory {},
}

impl Config {
    /// Reads and checks a configuration file.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        let config = Self::parse(&text).map_err(|error| match error {
            ConfigError::Parse { source, .. } => ConfigError::Parse {
                path: path.to_owned(),
                source,
            },
            other => other,
        })?;
        Ok(config)
    }

    /// Parses and checks configuration text.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(text).map_err(|source| ConfigError::Parse {
            path: PathBuf::new(),
            source: Box::new(source),
        })?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let hostname = &self.server.hostname;
        let label_ok = |label: &str| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        };
        if hostname.len() > 253 || !hostname.contains('.') || !hostname.split('.').all(label_ok) {
            return Err(ConfigError::Invalid(format!(
                "server.hostname: {hostname:?} is not a fully qualified host name"
            )));
        }
        if self.submission.max_message_size.0 == 0 {
            return Err(ConfigError::Invalid(
                "submission.max_message_size: must be greater than zero".to_owned(),
            ));
        }
        if self.imap.listen == self.submission.listen {
            return Err(ConfigError::Invalid(
                "imap.listen and submission.listen must differ".to_owned(),
            ));
        }
        Ok(())
    }
}

/// A size in bytes. Written as a number of bytes, or as text with a unit: `"512KB"`, `"40MB"`,
/// `"1GB"`. Units are powers of 1024.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByteSize(pub u64);

impl ByteSize {
    fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let digits = text.chars().take_while(char::is_ascii_digit).count();
        let number: u64 = text[..digits].parse().ok()?;
        let factor: u64 = match text[digits..].trim().to_ascii_uppercase().as_str() {
            "" | "B" => 1,
            "KB" => 1 << 10,
            "MB" => 1 << 20,
            "GB" => 1 << 30,
            _ => return None,
        };
        number.checked_mul(factor).map(Self)
    }
}

impl<'de> Deserialize<'de> for ByteSize {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct SizeVisitor;

        impl Visitor<'_> for SizeVisitor {
            type Value = ByteSize;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a number of bytes, or text such as \"40MB\"")
            }

            fn visit_i64<E: de::Error>(self, value: i64) -> Result<ByteSize, E> {
                u64::try_from(value)
                    .map(ByteSize)
                    .map_err(|_| E::custom("a size cannot be negative"))
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<ByteSize, E> {
                Ok(ByteSize(value))
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<ByteSize, E> {
                ByteSize::parse(value).ok_or_else(|| E::custom(format!("{value:?} is not a size")))
            }
        }

        deserializer.deserialize_any(SizeVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
        [server]
        hostname = "mail.example.org"

        [tls]
        certificate = "/etc/epistel/tls/fullchain.pem"
        private_key = "/etc/epistel/tls/key.pem"

        [metadata]
        backend = "memory"

        [blobs]
        backend = "memory"

        [inbound]
        backend = "memory"

        [outbound]
        backend = "memory"
    "#;

    fn error_of(text: &str) -> String {
        Config::parse(text).expect_err("should be refused").to_string()
    }

    #[test]
    fn minimal_configuration_gets_the_standard_ports() {
        let config = Config::parse(MINIMAL).unwrap();
        assert_eq!(config.server.hostname, "mail.example.org");
        assert_eq!(config.imap.listen.port(), 993);
        assert_eq!(config.submission.listen.port(), 465);
        assert_eq!(config.submission.max_message_size, ByteSize(40 << 20));
        assert_eq!(config.metadata, MetadataBackend::Memory {});
    }

    #[test]
    fn the_shipped_example_is_valid() {
        Config::parse(include_str!("../../../epistel.example.toml")).unwrap();
    }

    #[test]
    fn sizes_take_units_or_plain_bytes() {
        let with = |line: &str| Config::parse(&format!("{MINIMAL}\n[submission]\n{line}\n")).map(|c| c.submission);
        assert_eq!(
            with("max_message_size = \"10MB\"").unwrap().max_message_size,
            ByteSize(10 << 20)
        );
        assert_eq!(
            with("max_message_size = \"512 kb\"").unwrap().max_message_size,
            ByteSize(512 << 10)
        );
        assert_eq!(
            with("max_message_size = 1000").unwrap().max_message_size,
            ByteSize(1000)
        );
        assert!(with("max_message_size = \"lots\"").is_err());
        assert!(with("max_message_size = -1").is_err());
        assert!(with("max_message_size = 0").is_err());
    }

    #[test]
    fn unknown_keys_and_backends_are_errors() {
        assert!(error_of(&MINIMAL.replace("[tls]", "[tls]\nstarttls = true")).contains("starttls"));
        assert!(
            error_of(&MINIMAL.replace("[blobs]\n        backend = \"memory\"", "[blobs]\nbackend = \"tape\""))
                .contains("tape")
        );
        assert!(error_of(&format!("{MINIMAL}\n[pop3]\nlisten = \"[::]:995\"\n")).contains("pop3"));
        assert!(
            error_of(&MINIMAL.replace(
                "backend = \"memory\"\n\n        [blobs]",
                "backend = \"memory\"\ntable = \"x\"\n\n[blobs]"
            ))
            .contains("table")
        );
    }

    #[test]
    fn nonsense_values_are_refused() {
        assert!(error_of(&MINIMAL.replace("mail.example.org", "localhost")).contains("server.hostname"));
        let same_port = format!("{MINIMAL}\n[imap]\nlisten = \"[::]:465\"\n");
        assert!(error_of(&same_port).contains("must differ"));
    }
}
