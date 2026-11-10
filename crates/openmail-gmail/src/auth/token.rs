//! Access tokens.

use std::fmt;
use std::time::{Duration, Instant};

/// A short-lived OAuth access token and when it expires.
///
/// Access tokens are only ever held in memory. `Debug` does not print the secret.
#[derive(Clone, PartialEq, Eq)]
pub struct AccessToken {
    secret: String,
    expires_at: Option<Instant>,
}

impl AccessToken {
    /// A token that expires `expires_in` from now, or never if `None`.
    pub fn new(secret: impl Into<String>, expires_in: Option<Duration>) -> Self {
        Self {
            secret: secret.into(),
            expires_at: expires_in.and_then(|d| Instant::now().checked_add(d)),
        }
    }

    /// The bearer token to send in `Authorization: Bearer …`.
    pub fn secret(&self) -> &str {
        &self.secret
    }

    /// When the token expires, if known.
    pub fn expires_at(&self) -> Option<Instant> {
        self.expires_at
    }

    /// Whether the token has expired or will within `margin`.
    pub fn expires_within(&self, margin: Duration) -> bool {
        self.expires_at
            .is_some_and(|at| at.saturating_duration_since(Instant::now()) <= margin)
    }
}

impl fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccessToken")
            .field("secret", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_expiry() {
        let token = AccessToken::new("t", Some(Duration::from_secs(3599)));
        assert!(!token.expires_within(Duration::from_secs(60)));
        assert!(token.expires_within(Duration::from_secs(3600)));
        assert!(AccessToken::new("t", Some(Duration::ZERO)).expires_within(Duration::ZERO));
        assert!(!AccessToken::new("t", None).expires_within(Duration::from_secs(u64::MAX)));
    }

    #[test]
    fn debug_redacts_the_secret() {
        let debug = format!("{:?}", AccessToken::new("ya29.secret", None));
        assert!(!debug.contains("ya29"));
    }
}
