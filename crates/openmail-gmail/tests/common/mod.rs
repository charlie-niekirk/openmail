//! Shared helpers for the `wiremock` integration tests.

#![allow(dead_code)] // Each test binary uses a different subset.

pub mod batch;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use openmail_gmail::api::{GmailClient, RetryPolicy, TokenFuture, TokenProvider};
use reqwest::Url;
use wiremock::{MockServer, ResponseTemplate};

/// The base path every Gmail `users/me` call shares.
pub const USER: &str = "/gmail/v1/users/me";

/// A token provider that hands out `token-0`, then `token-1` after a refresh, …
#[derive(Default)]
pub struct TestTokens {
    generation: AtomicUsize,
    pub refreshes: AtomicUsize,
}

impl TestTokens {
    pub fn refreshes(&self) -> usize {
        self.refreshes.load(Ordering::SeqCst)
    }
}

impl TokenProvider for TestTokens {
    fn access_token(&self) -> TokenFuture<'_> {
        let n = self.generation.load(Ordering::SeqCst);
        Box::pin(async move { Ok(format!("token-{n}")) })
    }

    fn refresh_access_token<'a>(&'a self, _rejected: &'a str) -> TokenFuture<'a> {
        self.refreshes.fetch_add(1, Ordering::SeqCst);
        let n = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        Box::pin(async move { Ok(format!("token-{n}")) })
    }
}

/// A retry policy fast enough for tests.
pub fn fast_retries(max_retries: u32) -> RetryPolicy {
    RetryPolicy {
        max_retries,
        initial_backoff: Duration::from_millis(1),
        max_backoff: Duration::from_millis(5),
    }
}

/// A mock server, its client, and the client's token provider.
pub async fn setup() -> (MockServer, GmailClient, Arc<TestTokens>) {
    setup_with(fast_retries(3)).await
}

pub async fn setup_with(retry: RetryPolicy) -> (MockServer, GmailClient, Arc<TestTokens>) {
    let server = MockServer::start().await;
    let tokens = Arc::new(TestTokens::default());
    let client = GmailClient::builder(tokens.clone())
        .base_url(Url::parse(&server.uri()).unwrap())
        .retry_policy(retry)
        .build()
        .unwrap();
    (server, client, tokens)
}

/// The contents of `tests/fixtures/<name>`.
pub fn fixture(name: &str) -> String {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {path}: {e}"))
}

/// A JSON response with the given status and fixture body.
pub fn json(status: u16, name: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_raw(fixture(name), "application/json")
}
