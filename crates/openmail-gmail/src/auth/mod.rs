//! Google OAuth2 for installed apps.
//!
//! Openmail signs in with Google's installed-app flow: the system browser opens
//! Google's consent page, Google redirects to a one-shot loopback server on
//! `127.0.0.1`, and the authorization code is exchanged for tokens with PKCE.
//!
//! While the app is unverified by Google, anyone can bring their own OAuth client:
//! the client ID (and the optional secret Google issues to desktop clients) comes
//! from [`OAuthConfig`], usually built with [`OAuthConfig::from_env`].
//!
//! The refresh token is kept in a [`TokenStore`] (the OS keychain in the app), keyed
//! by the account's email address. Access tokens live only in memory: a
//! [`TokenProvider`] hands the REST client a valid one, refreshing it as needed and
//! again when the API rejects it.
//!
//! ```no_run
//! # async fn example() -> Result<(), openmail_gmail::auth::AuthError> {
//! use std::sync::Arc;
//!
//! use openmail_gmail::auth::{
//!     KeyringTokenStore, OAuthClient, OAuthConfig, RefreshingTokenProvider, TokenProvider,
//! };
//!
//! let client = OAuthClient::new(&OAuthConfig::from_env()?)?;
//! let store = Arc::new(KeyringTokenStore::new());
//!
//! // First run: sign in through the browser and keep the refresh token.
//! let authorization = client.sign_in().await?;
//! let provider =
//!     RefreshingTokenProvider::from_authorization(client.clone(), store.clone(), authorization)
//!         .await?;
//!
//! // Later runs: pick the account up from the store.
//! let provider = RefreshingTokenProvider::new(client, store, provider.account());
//! let token = provider.token().await?;
//! println!("Authorization: Bearer {}", token.secret());
//!
//! // The REST client takes it as a trait object.
//! let provider: Arc<dyn TokenProvider> = Arc::new(provider);
//! # let _ = provider;
//! # Ok(())
//! # }
//! ```

mod client;
mod config;
mod error;
mod loopback;
mod provider;
mod store;
mod token;

pub use client::{Authorization, OAuthClient};
pub use config::{CLIENT_ID_ENV, CLIENT_SECRET_ENV, OAuthConfig};
pub use error::AuthError;
pub use provider::{
    REFRESH_MARGIN, RefreshingTokenProvider, StaticTokenProvider, TokenError, TokenFuture,
    TokenProvider,
};
pub use store::{KeyringTokenStore, MemoryTokenStore, TokenStore};
pub use token::AccessToken;

/// Read, modify and label mail (not permanent deletion).
pub const SCOPE_GMAIL_MODIFY: &str = "https://www.googleapis.com/auth/gmail.modify";
/// Manage filters, send-as aliases and the vacation responder.
pub const SCOPE_GMAIL_SETTINGS_BASIC: &str = "https://www.googleapis.com/auth/gmail.settings.basic";

/// Every scope Openmail requests. `openid email` returns an ID token carrying the
/// account's email address, which keys the stored refresh token.
pub const SCOPES: [&str; 4] = [
    SCOPE_GMAIL_MODIFY,
    SCOPE_GMAIL_SETTINGS_BASIC,
    "openid",
    "email",
];
