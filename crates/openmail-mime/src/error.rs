//! Errors from parsing and decoding messages.

/// An error from `openmail-mime`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The text is not valid base64url.
    #[error("invalid base64url: {0}")]
    Base64(#[from] base64::DecodeError),
    /// The input holds no RFC 5322 message, such as empty input.
    #[error("not an RFC 5322 message")]
    NotAMessage,
}

/// A `Result` with [`Error`] as the error type.
pub type Result<T, E = Error> = std::result::Result<T, E>;
