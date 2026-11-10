//! Errors raised when constructing domain types from untrusted input.

/// An error from parsing or validating a domain value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A Gmail history ID was not a decimal unsigned integer.
    #[error("invalid history id {0:?}: expected an unsigned integer")]
    InvalidHistoryId(String),
}

/// A `Result` whose error defaults to [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;
