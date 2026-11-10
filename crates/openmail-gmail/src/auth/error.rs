//! Errors from signing in and managing tokens.

/// An error from the OAuth flow, token refresh or token storage.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AuthError {
    /// No OAuth client ID was configured.
    #[error(
        "no Google OAuth client ID: set {} or configure one",
        crate::auth::CLIENT_ID_ENV
    )]
    MissingClientId,
    /// The loopback redirect server could not listen or accept connections.
    #[error("OAuth loopback redirect server failed: {0}")]
    Loopback(#[source] std::io::Error),
    /// The redirect's `state` did not match the one sent, so it may be forged.
    #[error("OAuth redirect state did not match; sign-in was not completed")]
    StateMismatch,
    /// Google redirected back with an error, usually because the user declined.
    #[error("authorization was denied: {0}")]
    AuthorizationDenied(String),
    /// The redirect was malformed, for example missing the authorization code.
    #[error("invalid OAuth redirect: {0}")]
    InvalidRedirect(String),
}
