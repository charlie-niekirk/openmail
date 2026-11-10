//! Gmail REST API client: OAuth2 (PKCE with a loopback redirect), typed endpoints, batching and retries.

pub mod api;
pub mod auth;

pub use api::{GmailClient, TokenProvider};
