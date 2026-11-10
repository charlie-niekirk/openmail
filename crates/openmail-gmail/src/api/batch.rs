//! Gmail batch requests: many API calls in one `multipart/mixed` HTTP request.
//!
//! Gmail answers a batch with `200` and one embedded HTTP response per call, so
//! each call succeeds or fails on its own. [`GmailClient::batch`] splits the
//! input into batches of at most [`MAX_BATCH_SIZE`], matches responses to
//! requests by `Content-ID`, and resends only the calls that failed with a
//! retryable status.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use futures_util::{StreamExt as _, stream};
use openmail_core::{MessageId, ThreadId};
use reqwest::header::CONTENT_TYPE;
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;

use super::client::parse_retry_after;
use super::error::classify;
use super::models::format_query;
use super::quota;
use super::retry::Retrier;
use super::{ApiMessage, ApiThread, Error, GmailClient, MessageFormat, Result, ThreadFormat};

/// The most calls Gmail accepts in one batch. (The API allows 100, but Gmail
/// rate-limits batches larger than 50.)
pub const MAX_BATCH_SIZE: usize = 50;

/// Calls per batch by default. Gmail works through a batch's calls slowly
/// and enforces its per-user quota over short windows, so small batches spread
/// the load: 5 `threads.get` cost 50 units, 5 `messages.get` 25.
pub const DEFAULT_BATCH_SIZE: usize = 5;

/// Batch requests in flight at once by default. The quota limiter decides
/// when each is sent; three hide Gmail's batch latency (about a second) at the
/// default budget's pace even for 25-unit batches of `messages.get`.
pub const DEFAULT_BATCH_CONCURRENCY: usize = 3;

/// One call inside a batch: a method on a `users/me` resource.
#[derive(Debug, Clone)]
pub(crate) struct BatchRequest {
    method: Method,
    segments: Vec<String>,
    query: Vec<(&'static str, String)>,
    /// A JSON body.
    body: Option<Vec<u8>>,
    /// The call's Gmail quota units.
    cost: u32,
}

impl BatchRequest {
    /// A GET of `users/me/<segments>?<query>`.
    pub(crate) fn get(segments: &[&str], query: Vec<(&'static str, String)>) -> Self {
        Self {
            method: Method::GET,
            segments: segments.iter().map(|s| (*s).to_owned()).collect(),
            query,
            body: None,
            cost: quota::cost(&Method::GET, segments),
        }
    }
}

/// One embedded response from a batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ItemResponse {
    /// The `N` of `Content-ID: <response-item-N>`, if the part had one.
    pub(crate) content_id: Option<usize>,
    pub(crate) status: StatusCode,
    pub(crate) retry_after: Option<Duration>,
    pub(crate) body: Vec<u8>,
}

impl GmailClient {
    /// `threads.get` for many threads, batched. Returns one result per ID, in
    /// the order given; a thread that fails (for example with
    /// [`Error::NotFound`] after being deleted) does not fail the others.
    ///
    /// The outer `Err` is for a batch request that failed as a whole.
    pub async fn get_threads(
        &self,
        ids: &[ThreadId],
        format: ThreadFormat,
        metadata_headers: &[&str],
    ) -> Result<Vec<Result<ApiThread>>> {
        let requests = ids
            .iter()
            .map(|id| {
                BatchRequest::get(
                    &["threads", id.as_str()],
                    format_query(format.as_str(), metadata_headers),
                )
            })
            .collect();
        self.batch(requests).await
    }

    /// `messages.get` for many messages, batched. Returns one result per ID,
    /// in the order given, like [`get_threads`](Self::get_threads).
    pub async fn get_messages(
        &self,
        ids: &[MessageId],
        format: MessageFormat,
        metadata_headers: &[&str],
    ) -> Result<Vec<Result<ApiMessage>>> {
        let requests = ids
            .iter()
            .map(|id| {
                BatchRequest::get(
                    &["messages", id.as_str()],
                    format_query(format.as_str(), metadata_headers),
                )
            })
            .collect();
        self.batch(requests).await
    }

    /// Sends `requests` in batches of the client's batch size, with up to its
    /// batch concurrency in flight, and decodes each successful response as
    /// `T`. Results come back in the order of `requests`.
    ///
    /// Each batch is charged the quota units of all its calls before it is
    /// sent. Calls that fail with a retryable status (429, rate-limit 403, 5xx)
    /// are resent in a smaller batch after a backoff, per the client's
    /// [`RetryPolicy`](super::RetryPolicy). A `401` refreshes the token once per batch and
    /// resends the calls it rejected. Other failures become that item's `Err`.
    pub(crate) async fn batch<T: DeserializeOwned + Send>(
        &self,
        requests: Vec<BatchRequest>,
    ) -> Result<Vec<Result<T>>> {
        let mut results: Vec<Option<Result<T>>> = requests.iter().map(|_| None).collect();
        let indices: Vec<usize> = (0..requests.len()).collect();
        // Owned chunks: a stream over borrowed slices would make callers'
        // futures not `Send` (a higher-ranked lifetime limitation).
        let chunks: Vec<Vec<usize>> = indices
            .chunks(self.batch_size())
            .map(<[usize]>::to_vec)
            .collect();
        // Every batch is polled at once; the client's batch slots decide how
        // many are actually in flight (including retries).
        let all = chunks.len().max(1);
        let requests = &requests;
        let mut batches = stream::iter(chunks)
            .map(|chunk| self.run_batch(requests, chunk))
            .buffer_unordered(all);
        while let Some(batch) = batches.next().await {
            for (index, result) in batch? {
                results[index] = Some(result);
            }
        }
        Ok(results
            .into_iter()
            .enumerate()
            .map(|(i, result)| {
                result.unwrap_or_else(|| {
                    Err(Error::InvalidResponse(format!(
                        "batch response had no part for item {i}"
                    )))
                })
            })
            .collect())
    }

    /// Sends the `pending` requests as one batch, retrying failed items until
    /// each has a result, and returns them by request index.
    async fn run_batch<T: DeserializeOwned>(
        &self,
        requests: &[BatchRequest],
        mut pending: Vec<usize>,
    ) -> Result<Vec<(usize, Result<T>)>> {
        let mut results = Vec::with_capacity(pending.len());
        let mut retrier = Retrier::new(*self.retry_policy());
        let mut refreshed = false;

        loop {
            let permit = self.batch_slot().await;
            let started = Instant::now();
            let (responses, token, sent) = self.send_batch(requests, &pending).await?;
            let elapsed = started.elapsed();
            drop(permit);
            // Failed items by status and reason, for the debug log.
            let mut failures: BTreeMap<String, usize> = BTreeMap::new();
            let mut rate_limited_items = 0;

            let mut retry = Vec::new();
            let mut unauthorized = false;
            // Items that failed retryably, with their errors in case retries
            // are used up.
            let mut retryable = Vec::new();
            let mut rate_limited = false;
            let mut retry_after: Option<Duration> = None;
            for (index, response) in pending.iter().copied().zip(responses) {
                let Some(response) = response else {
                    results.push((
                        index,
                        Err(Error::InvalidResponse(format!(
                            "batch response had no part for item {index}"
                        ))),
                    ));
                    continue;
                };
                if response.status.is_success() {
                    results.push((
                        index,
                        serde_json::from_slice(&response.body).map_err(Into::into),
                    ));
                    continue;
                }
                if response.status == StatusCode::UNAUTHORIZED && !refreshed {
                    *failures.entry("401".to_owned()).or_default() += 1;
                    unauthorized = true;
                    retry.push(index);
                    continue;
                }
                let failure = classify(response.status, &response.body, response.retry_after);
                *failures
                    .entry(failure_label(response.status, &failure.error))
                    .or_default() += 1;
                rate_limited_items += usize::from(failure.is_rate_limit());
                if failure.retryable {
                    rate_limited |= failure.is_rate_limit();
                    retry_after = retry_after.max(response.retry_after);
                    retryable.push((index, failure.error));
                } else {
                    results.push((index, Err(failure.error)));
                }
            }

            if rate_limited_items > 0 {
                self.rate_limited(sent);
            }
            tracing::debug!(
                items = pending.len(),
                elapsed_ms = elapsed.as_millis(),
                failed = failures.values().sum::<usize>(),
                ?failures,
                quota_budget = self.stats().budget_per_minute,
                "Gmail batch"
            );

            let mut delay = None;
            if !retryable.is_empty() {
                delay = retrier.next_delay(rate_limited, retry_after);
                if delay.is_some() {
                    retry.extend(retryable.iter().map(|(index, _)| *index));
                    retry.sort_unstable();
                } else {
                    results.extend(
                        retryable
                            .into_iter()
                            .map(|(index, error)| (index, Err(error))),
                    );
                }
            }

            self.count(retry.len(), rate_limited_items);
            if retry.is_empty() {
                return Ok(results);
            }
            tracing::debug!(
                items = retry.len(),
                ?delay,
                rate_limited,
                "retrying Gmail batch items"
            );
            if unauthorized {
                refreshed = true;
                self.tokens()
                    .refresh_access_token(&token)
                    .await
                    .map_err(Error::Token)?;
            }
            if let Some(delay) = delay {
                tokio::time::sleep(delay).await;
            }
            pending = retry;
        }
    }

    /// Sends one batch of `requests[i]` for each `i` in `pending`, and returns
    /// each item's response (aligned with `pending`), the token used and when
    /// it was sent.
    async fn send_batch(
        &self,
        requests: &[BatchRequest],
        pending: &[usize],
    ) -> Result<(Vec<Option<ItemResponse>>, String, tokio::time::Instant)> {
        let boundary = format!("batch_openmail_{:016x}", fastrand::u64(..));
        let items: Vec<(usize, &BatchRequest)> =
            pending.iter().map(|&i| (i, &requests[i])).collect();
        let body = encode_batch(self, &boundary, &items);
        let content_type = format!("multipart/mixed; boundary={boundary}");
        let url = self.batch_url();
        let cost = items.iter().map(|(_, r)| r.cost).sum();

        let (response, token, sent) = self
            .send_with_token(cost, |http| {
                http.post(url.clone())
                    .header(CONTENT_TYPE, &content_type)
                    .body(body.clone())
            })
            .await?;
        let response_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let bytes = response.bytes().await?;
        let parts = parse_batch_response(&response_type, &bytes)?;

        // Match parts by Content-ID; a part without one takes its position.
        let mut by_index: HashMap<usize, ItemResponse> = HashMap::new();
        for (position, part) in parts.into_iter().enumerate() {
            let index = match part.content_id {
                Some(id) => id,
                None => match pending.get(position) {
                    Some(&index) => index,
                    None => continue,
                },
            };
            by_index.entry(index).or_insert(part);
        }
        let responses = pending.iter().map(|i| by_index.remove(i)).collect();
        Ok((responses, token, sent))
    }
}

/// A short label for a failed item, such as `429 rate limit` or
/// `400 invalidArgument`.
fn failure_label(status: StatusCode, error: &Error) -> String {
    match error {
        Error::Quota { kind, .. } => format!("{} {kind}", status.as_u16()),
        Error::Api {
            reason: Some(reason),
            ..
        } => format!("{} {reason}", status.as_u16()),
        _ => status.as_u16().to_string(),
    }
}

/// The `multipart/mixed` body for a batch. Each part's `Content-ID` is
/// `<item-N>`, where `N` is the request's index in the caller's input.
fn encode_batch(client: &GmailClient, boundary: &str, items: &[(usize, &BatchRequest)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (index, request) in items {
        let segments: Vec<&str> = request.segments.iter().map(String::as_str).collect();
        let mut url = client.user_url(&segments);
        if !request.query.is_empty() {
            url.query_pairs_mut().extend_pairs(&request.query);
        }
        let target = match url.query() {
            Some(query) => format!("{}?{query}", url.path()),
            None => url.path().to_owned(),
        };

        out.extend_from_slice(
            format!(
                "--{boundary}\r\n\
                 Content-Type: application/http\r\n\
                 Content-ID: <item-{index}>\r\n\
                 \r\n\
                 {} {target} HTTP/1.1\r\n",
                request.method
            )
            .as_bytes(),
        );
        match &request.body {
            Some(body) => {
                out.extend_from_slice(
                    format!(
                        "Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                );
                out.extend_from_slice(body);
                out.extend_from_slice(b"\r\n");
            }
            None => out.extend_from_slice(b"\r\n"),
        }
    }
    out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    out
}

/// Splits a batch response into its embedded HTTP responses, in the order
/// they appear.
pub(crate) fn parse_batch_response(content_type: &str, body: &[u8]) -> Result<Vec<ItemResponse>> {
    let boundary = boundary(content_type).ok_or_else(|| {
        Error::InvalidResponse(format!("batch response is not multipart: {content_type:?}"))
    })?;
    let delimiter = format!("--{boundary}").into_bytes();

    let mut parts = Vec::new();
    let mut starts = find_all(body, &delimiter)
        .into_iter()
        .filter(|&at| at == 0 || body[at - 1] == b'\n')
        .peekable();
    while let Some(start) = starts.next() {
        let after = &body[start + delimiter.len()..];
        if after.starts_with(b"--") {
            break; // The closing delimiter.
        }
        let end = starts.peek().copied().unwrap_or(body.len());
        let part = &body[start + delimiter.len()..end];
        // Drop the rest of the delimiter line, and the line break that belongs
        // to the next delimiter.
        let part = match part.iter().position(|&b| b == b'\n') {
            Some(newline) => &part[newline + 1..],
            None => continue,
        };
        parts.push(parse_part(strip_trailing_newline(part))?);
    }
    Ok(parts)
}

/// Parses one part: MIME headers, then an HTTP response.
fn parse_part(part: &[u8]) -> Result<ItemResponse> {
    let (headers, http) = split_head(part);
    let content_id = header_lines(headers)
        .find(|(name, _)| name.eq_ignore_ascii_case("content-id"))
        .and_then(|(_, value)| content_id_index(value));

    let (head, body) = split_head(trim_leading_newlines(http));
    let mut lines = header_lines_raw(head);
    let status_line = lines.next().unwrap_or_default();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .and_then(|code| StatusCode::from_u16(code).ok())
        .ok_or_else(|| {
            Error::InvalidResponse(format!("bad status line in batch part: {status_line:?}"))
        })?;
    let retry_after = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("retry-after"))
        .and_then(|(_, value)| parse_retry_after(value));

    Ok(ItemResponse {
        content_id,
        status,
        retry_after,
        body: strip_trailing_newline(body).to_vec(),
    })
}

/// `<response-item-7>` → `7`.
fn content_id_index(value: &str) -> Option<usize> {
    let value = value.trim().trim_start_matches('<').trim_end_matches('>');
    let value = value.strip_prefix("response-").unwrap_or(value);
    value.strip_prefix("item-")?.parse().ok()
}

/// The `boundary` parameter of a `multipart/*` content type.
fn boundary(content_type: &str) -> Option<&str> {
    let mut params = content_type.split(';');
    let mime = params.next()?.trim();
    if !mime.to_ascii_lowercase().starts_with("multipart/") {
        return None;
    }
    params
        .filter_map(|param| param.split_once('='))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("boundary"))
        .map(|(_, value)| value.trim().trim_matches('"'))
        .filter(|value| !value.is_empty())
}

/// Splits at the first blank line into the head and the body. With no blank
/// line, everything is head.
fn split_head(bytes: &[u8]) -> (&[u8], &[u8]) {
    let crlf = find(bytes, b"\r\n\r\n").map(|at| (at, 4));
    let lf = find(bytes, b"\n\n").map(|at| (at, 2));
    let split = match (crlf, lf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (a, b) => a.or(b),
    };
    match split {
        Some((at, len)) => (&bytes[..at], &bytes[at + len..]),
        None => (bytes, &[]),
    }
}

fn header_lines_raw(head: &[u8]) -> impl Iterator<Item = &str> {
    std::str::from_utf8(head)
        .unwrap_or_default()
        .lines()
        .map(|line| line.trim_end_matches('\r'))
}

fn header_lines(head: &[u8]) -> impl Iterator<Item = (&str, &str)> {
    header_lines_raw(head)
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim(), value.trim()))
}

fn trim_leading_newlines(mut bytes: &[u8]) -> &[u8] {
    while let [b'\r' | b'\n', rest @ ..] = bytes {
        bytes = rest;
    }
    bytes
}

fn strip_trailing_newline(bytes: &[u8]) -> &[u8] {
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    bytes.strip_suffix(b"\r").unwrap_or(bytes)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn find_all(haystack: &[u8], needle: &[u8]) -> Vec<usize> {
    haystack
        .windows(needle.len())
        .enumerate()
        .filter(|(_, w)| *w == needle)
        .map(|(at, _)| at)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const RESPONSE: &str = "--batch_abc\r\n\
        Content-Type: application/http\r\n\
        Content-ID: <response-item-1>\r\n\
        \r\n\
        HTTP/1.1 200 OK\r\n\
        Content-Type: application/json; charset=UTF-8\r\n\
        \r\n\
        {\"id\": \"b\"}\r\n\
        --batch_abc\r\n\
        Content-Type: application/http\r\n\
        Content-ID: <response-item-0>\r\n\
        \r\n\
        HTTP/1.1 429 Too Many Requests\r\n\
        Retry-After: 7\r\n\
        \r\n\
        {}\r\n\
        --batch_abc\r\n\
        Content-Type: application/http\r\n\
        \r\n\
        HTTP/1.1 204 No Content\r\n\
        \r\n\
        \r\n\
        --batch_abc--\r\n";

    #[test]
    fn parses_parts_in_order_with_status_ids_and_bodies() {
        let parts =
            parse_batch_response("multipart/mixed; boundary=batch_abc", RESPONSE.as_bytes())
                .unwrap();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].content_id, Some(1));
        assert_eq!(parts[0].status, StatusCode::OK);
        assert_eq!(parts[0].body, br#"{"id": "b"}"#);
        assert_eq!(parts[1].content_id, Some(0));
        assert_eq!(parts[1].status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(parts[1].retry_after, Some(Duration::from_secs(7)));
        assert_eq!(parts[2].content_id, None);
        assert_eq!(parts[2].status, StatusCode::NO_CONTENT);
        assert!(parts[2].body.is_empty());
    }

    #[test]
    fn tolerates_bare_newlines_and_a_quoted_boundary() {
        let lf = RESPONSE.replace("\r\n", "\n");
        let parts =
            parse_batch_response("multipart/mixed; boundary=\"batch_abc\"", lf.as_bytes()).unwrap();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].body, br#"{"id": "b"}"#);
    }

    #[test]
    fn a_non_multipart_response_is_an_error() {
        let err = parse_batch_response("application/json", b"{}").unwrap_err();
        assert!(matches!(err, Error::InvalidResponse(_)), "{err:?}");
    }

    #[test]
    fn content_ids_parse_with_or_without_the_response_prefix() {
        assert_eq!(content_id_index("<response-item-12>"), Some(12));
        assert_eq!(content_id_index("<item-3>"), Some(3));
        assert_eq!(content_id_index("<something-else>"), None);
    }
}
