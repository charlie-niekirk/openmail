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
    /// A configured OAuth endpoint was not a valid URL.
    #[error("invalid OAuth endpoint URL {url:?}: {source}")]
    InvalidEndpoint {
        /// The rejected URL.
        url: String,
        /// Why it was rejected.
        source: url::ParseError,
    },
    /// The system browser could not be opened.
    #[error("could not open the browser: {0}")]
    Browser(#[source] std::io::Error),
    /// The token endpoint could not be reached or did not answer.
    #[error("could not reach the OAuth token endpoint: {0}")]
    Http(#[from] reqwest::Error),
    /// The token endpoint rejected the request or sent a malformed response.
    #[error("OAuth token request failed: {0}")]
    TokenEndpoint(String),
    /// Google rejected the authorization code or refresh token (`invalid_grant`):
    /// the code expired, or the refresh token was revoked or expired.
    #[error("Google rejected the grant ({0}); sign in again")]
    InvalidGrant(String),
    /// Google did not issue a refresh token.
    #[error("Google did not return a refresh token")]
    MissingRefreshToken,
    /// The ID token was missing or did not carry a verified email address.
    #[error("could not read the account email from the ID token: {0}")]
    InvalidIdToken(String),
    /// The token store (usually the OS keychain) could not be read or written.
    #[error("token store failed: {0}")]
    TokenStore(#[source] Box<dyn std::error::Error + Send + Sync>),
}

impl AuthError {
    /// Whether the user has to sign in again to recover.
    pub fn requires_sign_in(&self) -> bool {
        matches!(self, Self::InvalidGrant(_) | Self::MissingRefreshToken)
    }
}
