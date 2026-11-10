//! The HTTP client: base URL, authentication, and the retry loop.

use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE, RETRY_AFTER};
use reqwest::{Method, RequestBuilder, Response, StatusCode, Url};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::error::{Error, Result, classify};
use super::quota::{self, QuotaLimiter};
use super::retry::Retrier;
use super::{QuotaLimit, RetryPolicy, TokenProvider};

/// A typed client for the Gmail REST API (`gmail/v1`), acting as the signed-in
/// user (`users/me`).
///
/// Cloning is cheap and shares the connection pool and quota limiter. Every
/// request gets its access token from the [`TokenProvider`]. A `401` triggers
/// one token refresh and a retry; `429`, rate-limit `403`s, `5xx` and timeouts
/// are retried with exponential backoff and jitter, per the [`RetryPolicy`].
/// Before each attempt the request's Gmail quota cost is charged to the
/// [`QuotaLimit`], waiting if needed, so a busy client stays under Gmail's
/// per-user rate limit.
#[derive(Clone)]
pub struct GmailClient {
    inner: Arc<Inner>,
}

struct Inner {
    http: reqwest::Client,
    base_url: Url,
    tokens: Arc<dyn TokenProvider>,
    retry: RetryPolicy,
    quota: Option<QuotaLimiter>,
}

impl std::fmt::Debug for GmailClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GmailClient")
            .field("base_url", &self.inner.base_url.as_str())
            .field("retry", &self.inner.retry)
            .field("quota", &self.inner.quota.as_ref().map(QuotaLimiter::limit))
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
            quota: Some(QuotaLimit::default()),
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

    /// The batch endpoint, `batch/gmail/v1`.
    pub(crate) fn batch_url(&self) -> Url {
        let mut url = self.inner.base_url.clone();
        url.path_segments_mut()
            .expect("the builder only accepts http(s) base URLs")
            .pop_if_empty()
            .extend(["batch", "gmail", "v1"]);
        url
    }

    pub(crate) fn tokens(&self) -> &dyn TokenProvider {
        &*self.inner.tokens
    }

    pub(crate) fn retry_policy(&self) -> &RetryPolicy {
        &self.inner.retry
    }

    /// Sends a GET for a `users/me` resource and decodes the JSON response.
    pub(crate) async fn get_json<T: DeserializeOwned>(
        &self,
        segments: &[&str],
        query: &[(&str, String)],
    ) -> Result<T> {
        let url = self.user_url(segments);
        let cost = quota::cost(&Method::GET, segments);
        let response = self
            .send(cost, |http| http.get(url.clone()).query(query))
            .await?;
        let bytes = response.bytes().await?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// Sends `method` to a `users/me` resource with an optional JSON body, and
    /// decodes the JSON response.
    pub(crate) async fn call_json<T: DeserializeOwned>(
        &self,
        method: Method,
        segments: &[&str],
        body: Option<&(impl Serialize + ?Sized)>,
    ) -> Result<T> {
        let bytes = self.call(method, segments, body).await?.bytes().await?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// Like [`call_json`](Self::call_json), for calls whose success response
    /// has no body we need.
    pub(crate) async fn call_empty(
        &self,
        method: Method,
        segments: &[&str],
        body: Option<&(impl Serialize + ?Sized)>,
    ) -> Result<()> {
        self.call(method, segments, body).await?;
        Ok(())
    }

    async fn call(
        &self,
        method: Method,
        segments: &[&str],
        body: Option<&(impl Serialize + ?Sized)>,
    ) -> Result<Response> {
        let url = self.user_url(segments);
        let cost = quota::cost(&method, segments);
        let body = body.map(serde_json::to_vec).transpose()?;
        self.send(cost, |http| {
            let request = http.request(method.clone(), url.clone());
            match &body {
                Some(body) => request
                    .header(CONTENT_TYPE, "application/json")
                    .body(body.clone()),
                // Google may answer a body-less POST without a length with 411.
                None => request.header(CONTENT_LENGTH, 0),
            }
        })
        .await
    }

    /// Sends a request built by `build`, adding the access token, refreshing it
    /// once on `401` and retrying retryable failures. Returns the successful
    /// response; any other status becomes an [`Error`].
    ///
    /// Each attempt is charged `cost` quota units first. `build` runs once per
    /// attempt, so it must build the same request each time.
    pub(crate) async fn send(
        &self,
        cost: u32,
        build: impl Fn(&reqwest::Client) -> RequestBuilder,
    ) -> Result<Response> {
        self.send_with_token(cost, build)
            .await
            .map(|(response, _)| response)
    }

    /// Waits until `cost` quota units may be spent.
    pub(crate) async fn charge(&self, cost: u32) {
        if let Some(quota) = &self.inner.quota {
            quota.acquire(cost).await;
        }
    }

    /// Like [`send`](Self::send), but also returns the access token the
    /// successful attempt used, so a caller that sees a `401` inside the
    /// response (as in a batch) can refresh exactly that token.
    pub(crate) async fn send_with_token(
        &self,
        cost: u32,
        build: impl Fn(&reqwest::Client) -> RequestBuilder,
    ) -> Result<(Response, String)> {
        let inner = &*self.inner;
        let mut token = inner.tokens.access_token().await.map_err(Error::Token)?;
        let mut refreshed = false;
        let mut retrier = Retrier::new(inner.retry);

        loop {
            self.charge(cost).await;
            let response = match build(&inner.http).bearer_auth(&token).send().await {
                Ok(response) => response,
                Err(err) if err.is_timeout() => match retrier.next_delay(false, None) {
                    Some(delay) => {
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    None => return Err(err.into()),
                },
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
            if failure.retryable
                && let Some(delay) = retrier.next_delay(failure.is_rate_limit(), retry_after)
            {
                tokio::time::sleep(delay).await;
                continue;
            }
            return Err(failure.error);
        }
    }
}

/// `Retry-After` in seconds. (Google does not send the HTTP-date form.)
fn retry_after(response: &Response) -> Option<Duration> {
    parse_retry_after(response.headers().get(RETRY_AFTER)?.to_str().ok()?)
}

/// Parses a `Retry-After` value in seconds.
pub(crate) fn parse_retry_after(value: &str) -> Option<Duration> {
    value.trim().parse().ok().map(Duration::from_secs)
}

/// Configures a [`GmailClient`].
#[must_use]
pub struct GmailClientBuilder {
    tokens: Arc<dyn TokenProvider>,
    base_url: Option<Url>,
    http: Option<reqwest::Client>,
    retry: RetryPolicy,
    quota: Option<QuotaLimit>,
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

    /// Sets how fast the client may spend Gmail quota units, or `None` for no
    /// client-side limit. Defaults to [`QuotaLimit::default`].
    pub fn quota_limit(mut self, limit: Option<QuotaLimit>) -> Self {
        self.quota = limit;
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
                quota: self.quota.map(QuotaLimiter::new),
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
