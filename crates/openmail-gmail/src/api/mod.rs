//! A typed client for the Gmail REST API.
//!
//! [`GmailClient`] wraps `reqwest` and returns serde models that mirror Gmail's
//! JSON resources. Those convert into `openmail-core` domain types.

mod client;
mod endpoints;
mod error;
mod models;
mod retry;

pub use crate::auth::{TokenError, TokenFuture, TokenProvider};
pub use client::{GmailClient, GmailClientBuilder, default_http_client};
pub use error::{Error, QuotaKind, Result};
pub use models::Profile;
pub use retry::RetryPolicy;
