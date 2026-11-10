//! The HTTP client: base URL, authentication, and the retry loop.

use std::sync::Arc;
use std::time::Duration;

use reqwest::header::RETRY_AFTER;
use reqwest::{RequestBuilder, Response, StatusCode, Url};
use serde::de::DeserializeOwned;

use super::error::{Error, Result, classify};
use super::{RetryPolicy, TokenProvider};

/// A typed client for the Gmail REST API (`gmail/v1`), acting as the signed-in
/// user (`users/me`).
///
/// Cloning is cheap and shares the connection pool. Every request gets its
/// access token from the [`TokenProvider`]. A `401` triggers one token refresh
/// and a retry; `429`, rate-limit `403`s, `5xx` and timeouts are retried with
/// exponential backoff and jitter, per the [`RetryPolicy`].
#[derive(Clone)]
pub struct GmailClient {
    inner: Arc<Inner>,
}

struct Inner {
    http: reqwest::Client,
    base_url: Url,
    tokens: Arc<dyn TokenProvider>,
    retry: RetryPolicy,
}

impl std::fmt::Debug for GmailClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GmailClient")
            .field("base_url", &self.inner.base_url.as_str())
            .field("retry", &self.inner.retry)
            .finish_non_exhaustive()
    }
}

impl GmailClient {
    /// Google's API host. Paths such as `gmail/v1/users/me/labels` are joined onto it.
    pub const DEFAULT_BASE_URL: &'static str = "https://gmail.googleapis.com/";

    /// A client for the real Gmail API with the default retry policy.
    pub fn new(tokens: Arc<dyn TokenProvider>) -> Result<Self> {
        Self::builder(tokens).build()
    }

    /// Starts configuring a client.
    pub fn builder(tokens: Arc<dyn TokenProvider>) -> GmailClientBuilder {
        GmailClientBuilder {
            tokens,
            base_url: None,
            http: None,
            retry: RetryPolicy::default(),
        }
    }

    /// The URL of a `users/me` resource, such as `["threads", id]`. Each segment
    /// is percent-encoded.
    pub(crate) fn user_url(&self, segments: &[&str]) -> Url {
        let mut url = self.inner.base_url.clone();
        url.path_segments_mut()
            .expect("the builder only accepts http(s) base URLs")
            .pop_if_empty()
            .extend(["gmail", "v1", "users", "me"])
            .extend(segments);
        url
    }

    /// Sends a GET for a `users/me` resource and decodes the JSON response.
    pub(crate) async fn get_json<T: DeserializeOwned>(
        &self,
        segments: &[&str],
        query: &[(&str, String)],
    ) -> Result<T> {
        let url = self.user_url(segments);
        let response = self.send(|http| http.get(url.clone()).query(query)).await?;
        let bytes = response.bytes().await?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// Sends a request built by `build`, adding the access token, refreshing it
    /// once on `401` and retrying retryable failures. Returns the successful
    /// response; any other status becomes an [`Error`].
    ///
    /// `build` runs once per attempt, so it must build the same request each time.
    pub(crate) async fn send(
        &self,
        build: impl Fn(&reqwest::Client) -> RequestBuilder,
    ) -> Result<Response> {
        self.send_with_token(build)
            .await
            .map(|(response, _)| response)
    }

    /// Like [`send`](Self::send), but also returns the access token the
    /// successful attempt used, so a caller that sees a `401` inside the
    /// response (as in a batch) can refresh exactly that token.
    pub(crate) async fn send_with_token(
        &self,
        build: impl Fn(&reqwest::Client) -> RequestBuilder,
    ) -> Result<(Response, String)> {
        let inner = &*self.inner;
        let mut token = inner.tokens.access_token().await.map_err(Error::Token)?;
        let mut refreshed = false;
        let mut attempt = 0;

        loop {
            let response = match build(&inner.http).bearer_auth(&token).send().await {
                Ok(response) => response,
                Err(err) if err.is_timeout() && attempt < inner.retry.max_retries => {
                    tokio::time::sleep(inner.retry.backoff(attempt, None)).await;
                    attempt += 1;
                    continue;
                }
                Err(err) => return Err(err.into()),
            };

            let status = response.status();
            if status.is_success() {
                return Ok((response, token));
            }
            if status == StatusCode::UNAUTHORIZED && !refreshed {
                refreshed = true;
                token = inner
                    .tokens
                    .refresh_access_token(&token)
                    .await
                    .map_err(Error::Token)?;
                continue;
            }

            let retry_after = retry_after(&response);
            let body = response.bytes().await.unwrap_or_default();
            let failure = classify(status, &body, retry_after);
            if failure.retryable && attempt < inner.retry.max_retries {
                tokio::time::sleep(inner.retry.backoff(attempt, retry_after)).await;
                attempt += 1;
                continue;
            }
            return Err(failure.error);
        }
    }
}

/// `Retry-After` in seconds. (Google does not send the HTTP-date form.)
fn retry_after(response: &Response) -> Option<Duration> {
    let value = response.headers().get(RETRY_AFTER)?.to_str().ok()?;
    value.trim().parse().ok().map(Duration::from_secs)
}

/// Configures a [`GmailClient`].
#[must_use]
pub struct GmailClientBuilder {
    tokens: Arc<dyn TokenProvider>,
    base_url: Option<Url>,
    http: Option<reqwest::Client>,
    retry: RetryPolicy,
}

impl GmailClientBuilder {
    /// Sends requests to `url` instead of [`GmailClient::DEFAULT_BASE_URL`],
    /// such as a `wiremock` server in tests.
    pub fn base_url(mut self, url: Url) -> Self {
        self.base_url = Some(url);
        self
    }

    /// Uses this `reqwest` client instead of [`default_http_client`].
    pub fn http_client(mut self, http: reqwest::Client) -> Self {
        self.http = Some(http);
        self
    }

    /// Sets how failed requests are retried.
    pub fn retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Builds the client.
    pub fn build(self) -> Result<GmailClient> {
        let base_url = match self.base_url {
            Some(url) => url,
            None => Url::parse(GmailClient::DEFAULT_BASE_URL).expect("valid default URL"),
        };
        if !matches!(base_url.scheme(), "http" | "https") || base_url.cannot_be_a_base() {
            return Err(Error::InvalidConfig(format!(
                "base URL must be http(s): {base_url}"
            )));
        }
        let http = match self.http {
            Some(http) => http,
            None => default_http_client()?,
        };
        Ok(GmailClient {
            inner: Arc::new(Inner {
                http,
                base_url,
                tokens: self.tokens,
                retry: self.retry,
            }),
        })
    }
}

/// A `reqwest` client for Google APIs: rustls with the `ring` provider, and
/// connect and request timeouts.
///
/// Installs `ring` as the process-wide rustls provider if none is installed yet,
/// since `reqwest` is built without its own.
pub fn default_http_client() -> Result<reqwest::Client> {
    // Fails only if a provider is already installed, which is fine.
    let _ = rustls::crypto::ring::default_provider().install_default();
    Ok(reqwest::Client::builder()
        .user_agent(concat!("openmail/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(60))
        .build()?)
}
