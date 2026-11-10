//! The error type shared by the storage, inbound and notification traits.

/// A boxed error from a backend, kept opaque so that no vendor type crosses a trait.
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Result with the crate's [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

/// What a backend operation can report.
///
/// The first three variants are answers: the caller asked for something the data does not allow.
/// [`Error::Unavailable`] is a fault: the backend could not be reached or misbehaved, and the same
/// call may succeed later.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The thing named by the call does not exist.
    #[error("not found")]
    NotFound,
    /// The thing the call would create exists already.
    #[error("already exists")]
    AlreadyExists,
    /// The call is refused because of the current state; the text says which rule.
    #[error("conflict: {0}")]
    Conflict(&'static str),
    /// The backend failed.
    #[error("backend unavailable: {0}")]
    Unavailable(#[source] BoxError),
}

impl Error {
    /// Wraps a backend failure.
    pub fn unavailable(source: impl Into<BoxError>) -> Self {
        Self::Unavailable(source.into())
    }
}
