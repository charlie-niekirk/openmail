//! OAuth client configuration, including "bring your own client ID".

use std::fmt;

use super::AuthError;

/// Environment variable holding the Google OAuth client ID.
pub const CLIENT_ID_ENV: &str = "OPENMAIL_GOOGLE_CLIENT_ID";
/// Environment variable holding the Google OAuth client secret, if the client has one.
pub const CLIENT_SECRET_ENV: &str = "OPENMAIL_GOOGLE_CLIENT_SECRET";

/// Which Google OAuth client to sign in with, and the endpoints to use.
///
/// Google issues a client secret to "Desktop app" clients, but it cannot be kept
/// secret in a distributed binary; PKCE protects the flow instead. It is optional
/// here because Google still requires it at the token endpoint for those clients.
#[derive(Clone, PartialEq, Eq)]
pub struct OAuthConfig {
    /// The OAuth client ID.
    pub client_id: String,
    /// The client secret, for clients that have one.
    pub client_secret: Option<String>,
    /// The authorization endpoint the browser is sent to.
    pub auth_url: String,
    /// The token endpoint for code exchange and refresh.
    pub token_url: String,
}

impl OAuthConfig {
    /// Google's authorization endpoint.
    pub const GOOGLE_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
    /// Google's token endpoint.
    pub const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

    /// A configuration for `client_id` against Google's endpoints.
    pub fn new(client_id: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            client_secret: None,
            auth_url: Self::GOOGLE_AUTH_URL.to_owned(),
            token_url: Self::GOOGLE_TOKEN_URL.to_owned(),
        }
    }

    /// Sets the client secret.
    #[must_use]
    pub fn with_client_secret(mut self, secret: impl Into<String>) -> Self {
        self.client_secret = Some(secret.into());
        self
    }

    /// Replaces the authorization and token endpoints, for example with a mock
    /// server in tests.
    #[must_use]
    pub fn with_endpoints(
        mut self,
        auth_url: impl Into<String>,
        token_url: impl Into<String>,
    ) -> Self {
        self.auth_url = auth_url.into();
        self.token_url = token_url.into();
        self
    }

    /// Reads the client from [`CLIENT_ID_ENV`] and [`CLIENT_SECRET_ENV`].
    ///
    /// # Errors
    ///
    /// [`AuthError::MissingClientId`] if the client ID is unset or blank.
    pub fn from_env() -> Result<Self, AuthError> {
        Self::from_env_or(None)
    }

    /// Reads the client from the environment, falling back to `config` (for example
    /// from the user's settings) when [`CLIENT_ID_ENV`] is unset.
    ///
    /// The environment wins so a developer can override a configured client
    /// without editing settings.
    ///
    /// # Errors
    ///
    /// [`AuthError::MissingClientId`] if neither provides a client ID.
    pub fn from_env_or(config: Option<Self>) -> Result<Self, AuthError> {
        Self::from_lookup(|name| std::env::var(name).ok(), config)
    }

    fn from_lookup(
        lookup: impl Fn(&str) -> Option<String>,
        fallback: Option<Self>,
    ) -> Result<Self, AuthError> {
        let non_blank = |name| {
            lookup(name)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };
        match non_blank(CLIENT_ID_ENV) {
            Some(client_id) => {
                let mut config = Self::new(client_id);
                config.client_secret = non_blank(CLIENT_SECRET_ENV);
                Ok(config)
            }
            None => fallback
                .filter(|c| !c.client_id.trim().is_empty())
                .ok_or(AuthError::MissingClientId),
        }
    }
}

impl fmt::Debug for OAuthConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthConfig")
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "<redacted>"),
            )
            .field("auth_url", &self.auth_url)
            .field("token_url", &self.token_url)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn lookup(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |name| vars.get(name).cloned()
    }

    #[test]
    fn reads_client_id_and_secret_from_env() {
        let config = OAuthConfig::from_lookup(
            lookup(&[(CLIENT_ID_ENV, " id.apps "), (CLIENT_SECRET_ENV, "s3cret")]),
            None,
        )
        .unwrap();
        assert_eq!(config.client_id, "id.apps");
        assert_eq!(config.client_secret.as_deref(), Some("s3cret"));
        assert_eq!(config.auth_url, OAuthConfig::GOOGLE_AUTH_URL);
        assert_eq!(config.token_url, OAuthConfig::GOOGLE_TOKEN_URL);
    }

    #[test]
    fn secret_is_optional_and_blank_means_unset() {
        let config = OAuthConfig::from_lookup(
            lookup(&[(CLIENT_ID_ENV, "id"), (CLIENT_SECRET_ENV, "  ")]),
            None,
        )
        .unwrap();
        assert_eq!(config.client_secret, None);
    }

    #[test]
    fn env_overrides_configured_client() {
        let configured = OAuthConfig::new("from-settings").with_client_secret("x");
        let config =
            OAuthConfig::from_lookup(lookup(&[(CLIENT_ID_ENV, "from-env")]), Some(configured))
                .unwrap();
        assert_eq!(config.client_id, "from-env");
        assert_eq!(config.client_secret, None);
    }

    #[test]
    fn falls_back_to_configured_client() {
        let configured = OAuthConfig::new("from-settings").with_client_secret("x");
        let config = OAuthConfig::from_lookup(lookup(&[]), Some(configured.clone())).unwrap();
        assert_eq!(config, configured);
    }

    #[test]
    fn missing_client_id_is_an_error() {
        assert!(matches!(
            OAuthConfig::from_lookup(lookup(&[(CLIENT_ID_ENV, "")]), None),
            Err(AuthError::MissingClientId)
        ));
        assert!(matches!(
            OAuthConfig::from_lookup(lookup(&[]), Some(OAuthConfig::new(" "))),
            Err(AuthError::MissingClientId)
        ));
    }

    #[test]
    fn debug_redacts_the_secret() {
        let debug = format!("{:?}", OAuthConfig::new("id").with_client_secret("s3cret"));
        assert!(!debug.contains("s3cret"));
        assert!(debug.contains("<redacted>"));
    }
}
