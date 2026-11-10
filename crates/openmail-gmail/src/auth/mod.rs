//! Google OAuth2 for installed apps.
//!
//! Openmail signs in with Google's installed-app flow: the system browser opens
//! Google's consent page, Google redirects to a one-shot loopback server on
//! `127.0.0.1`, and the authorization code is exchanged for tokens with PKCE.
//!
//! While the app is unverified by Google, anyone can bring their own OAuth client:
//! the client ID (and the optional secret Google issues to desktop clients) comes
//! from [`OAuthConfig`], usually built with [`OAuthConfig::from_env`].

mod client;
mod config;
mod error;
mod loopback;
mod token;

pub use client::{Authorization, OAuthClient};
pub use config::{CLIENT_ID_ENV, CLIENT_SECRET_ENV, OAuthConfig};
pub use error::AuthError;
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
