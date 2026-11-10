//! A fake Gmail mailbox served by `wiremock`.

#![allow(dead_code)] // Each test binary uses a different subset.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use openmail_gmail::api::{GmailClient, QuotaPacing, RetryPolicy, TokenFuture, TokenProvider};
use serde_json::{Value, json};
use url::Url;
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockBuilder, MockServer, Request, Respond, ResponseTemplate};

pub const USER: &str = "/gmail/v1/users/me";
pub const BATCH: &str = "/batch/gmail/v1";

/// The history ID the fake profile reports.
pub const PROFILE_HISTORY_ID: u64 = 5000;

/// The newest message date: 2026-11-01 00:00 UTC, in milliseconds.
pub const NEWEST_MS: i64 = 1_793_491_200_000;

/// Hands out a fixed access token.
struct StaticToken;

impl TokenProvider for StaticToken {
    fn access_token(&self) -> TokenFuture<'_> {
        Box::pin(async { Ok("token".to_owned()) })
    }

    fn refresh_access_token<'a>(&'a self, _rejected: &'a str) -> TokenFuture<'a> {
        Box::pin(async { Ok("token".to_owned()) })
    }
}

/// A mock server and a client for it that does not retry.
pub async fn setup() -> (MockServer, GmailClient) {
    setup_with(RetryPolicy::none()).await
}

/// A mock server and a client for it that retries quickly.
pub async fn setup_retrying() -> (MockServer, GmailClient) {
    setup_with(fast_retries()).await
}

async fn setup_with(retry: RetryPolicy) -> (MockServer, GmailClient) {
    setup_paced(QuotaPacing::Off, retry).await
}

/// A mock server and a client for it with this quota pacing and retry
/// policy. (The other setups have no quota limiter, so rate limits in tests
/// do not lock the client out.)
pub async fn setup_paced(pacing: QuotaPacing, retry: RetryPolicy) -> (MockServer, GmailClient) {
    let server = MockServer::start().await;
    let client = GmailClient::builder(Arc::new(StaticToken))
        .base_url(Url::parse(&server.uri()).unwrap())
        .retry_policy(retry)
        .quota_pacing(pacing)
        .build()
        .unwrap();
    (server, client)
}

/// The retry policy of [`setup_retrying`].
pub fn fast_retries() -> RetryPolicy {
    RetryPolicy {
        max_retries: 3,
        initial_backoff: Duration::from_millis(1),
        max_backoff: Duration::from_millis(5),
        rate_limit_initial_backoff: Duration::from_millis(1),
        rate_limit_max_backoff: Duration::from_millis(5),
        rate_limit_max_wait: Duration::from_millis(50),
    }
}

/// `GET users/me/<resource>`.
pub fn get(resource: &str) -> MockBuilder {
    Mock::given(method("GET")).and(path(format!("{USER}/{resource}")))
}

/// Mounts `users.getProfile`, expected exactly `times` times.
pub async fn mount_profile(server: &MockServer, times: u64) {
    get("profile")
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "emailAddress": "ada@example.com",
            "messagesTotal": 10,
            "threadsTotal": 5,
            "historyId": PROFILE_HISTORY_ID.to_string(),
        })))
        .expect(times)
        .mount(server)
        .await;
}

/// Mounts `labels.list` with INBOX, UNREAD and one user label.
pub async fn mount_labels(server: &MockServer) {
    get("labels")
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "labels": [
                {"id": "INBOX", "name": "INBOX", "type": "system"},
                {"id": "UNREAD", "name": "UNREAD", "type": "system"},
                {"id": "Label_1", "name": "Work", "type": "user",
                 "color": {"textColor": "#000000", "backgroundColor": "#16a765"}},
            ]
        })))
        .mount(server)
        .await;
}

/// Which listing a `threads.list` mock answers.
#[derive(Debug, Clone, Copy)]
pub enum Listing {
    Inbox,
    Backfill,
}

/// A `threads.list` mock for one page of `listing`: the first page when
/// `token` is `None`.
pub fn list_page(listing: Listing, token: Option<&str>) -> MockBuilder {
    let mock = get("threads");
    let mock = match listing {
        Listing::Inbox => mock.and(query_param("labelIds", "INBOX")),
        Listing::Backfill => mock.and(query_param_is_missing("labelIds")),
    };
    match token {
        Some(token) => mock.and(query_param("pageToken", token)),
        None => mock.and(query_param_is_missing("pageToken")),
    }
}

/// A `threads.list` response listing `threads` (IDs from the mailbox).
pub fn page(mailbox: &Mailbox, threads: &[&str], next: Option<&str>) -> ResponseTemplate {
    let threads: Vec<Value> = threads
        .iter()
        .map(|id| {
            let t = &mailbox.threads[*id];
            json!({"id": id, "snippet": "", "historyId": t.history_id.to_string()})
        })
        .collect();
    let mut body = json!({"threads": threads, "resultSizeEstimate": threads.len()});
    if let Some(next) = next {
        body["nextPageToken"] = json!(next);
    }
    ResponseTemplate::new(200).set_body_json(body)
}

/// A thread in the fake mailbox.
#[derive(Debug, Clone)]
pub struct FakeThread {
    pub history_id: u64,
    /// `(message ID, label IDs, internal date in ms)`, oldest first.
    pub messages: Vec<(String, Vec<String>, i64)>,
}

/// The fake mailbox the batch endpoint serves.
#[derive(Debug, Clone, Default)]
pub struct Mailbox {
    pub threads: HashMap<String, FakeThread>,
}

impl Mailbox {
    /// Adds thread `tN` with one message `mN`, `N` days older than
    /// [`NEWEST_MS`], carrying `labels`.
    pub fn with(mut self, n: u32, labels: &[&str]) -> Self {
        let date = NEWEST_MS - i64::from(n) * 86_400_000;
        self.threads.insert(
            format!("t{n}"),
            FakeThread {
                history_id: 100 + u64::from(n),
                messages: vec![(
                    format!("m{n}"),
                    labels.iter().map(|l| (*l).to_owned()).collect(),
                    date,
                )],
            },
        );
        self
    }

    /// Adds thread `sN` with one message whose ID is also `sN`, as Gmail
    /// gives a thread its first message's ID; otherwise like [`with`](Self::with).
    pub fn with_single(mut self, n: u32, labels: &[&str]) -> Self {
        let date = NEWEST_MS - i64::from(n) * 86_400_000;
        self.threads.insert(
            format!("s{n}"),
            FakeThread {
                history_id: 100 + u64::from(n),
                messages: vec![(
                    format!("s{n}"),
                    labels.iter().map(|l| (*l).to_owned()).collect(),
                    date,
                )],
            },
        );
        self
    }

    /// Adds `thread` with `messages` (`(ID, labels, days older than
    /// NEWEST_MS)`, oldest first).
    pub fn with_thread(mut self, thread: &str, messages: &[(&str, &[&str], u32)]) -> Self {
        self.threads.insert(
            thread.to_owned(),
            FakeThread {
                history_id: 200 + self.threads.len() as u64,
                messages: messages
                    .iter()
                    .map(|(id, labels, days)| {
                        (
                            (*id).to_owned(),
                            labels.iter().map(|l| (*l).to_owned()).collect(),
                            NEWEST_MS - i64::from(*days) * 86_400_000,
                        )
                    })
                    .collect(),
            },
        );
        self
    }

    fn message_json(
        thread_id: &str,
        thread: &FakeThread,
        (id, labels, date): &(String, Vec<String>, i64),
    ) -> Value {
        json!({
            "id": id,
            "threadId": thread_id,
            "labelIds": labels,
            "snippet": format!("snippet of {id}"),
            "historyId": thread.history_id.to_string(),
            "internalDate": date.to_string(),
            "sizeEstimate": 1234,
            "payload": {
                "mimeType": "text/plain",
                "headers": [
                    {"name": "From", "value": "Ada Lovelace <ada@example.com>"},
                    {"name": "To", "value": "bob@example.com"},
                    {"name": "Subject", "value": format!("Subject {thread_id}")},
                    {"name": "Message-ID", "value": format!("<{id}@example.com>")},
                ]
            }
        })
    }

    fn thread_json(&self, id: &str) -> Option<Value> {
        let thread = self.threads.get(id)?;
        let messages: Vec<Value> = thread
            .messages
            .iter()
            .map(|message| Self::message_json(id, thread, message))
            .collect();
        Some(json!({
            "id": id,
            "historyId": thread.history_id.to_string(),
            "messages": messages,
        }))
    }

    fn metadata_message_json(&self, id: &str) -> Option<Value> {
        self.threads.iter().find_map(|(thread_id, thread)| {
            let message = thread.messages.iter().find(|(m, _, _)| m == id)?;
            Some(Self::message_json(thread_id, thread, message))
        })
    }

    /// Every message as `(message ID, thread ID, date)`, newest first, as
    /// `messages.list` orders them.
    pub fn listing(&self) -> Vec<(String, String, i64)> {
        let mut all: Vec<(String, String, i64)> = self
            .threads
            .iter()
            .flat_map(|(thread_id, thread)| {
                thread
                    .messages
                    .iter()
                    .map(|(id, _, date)| (id.clone(), thread_id.clone(), *date))
            })
            .collect();
        all.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
        all
    }

    fn raw_message_json(&self, id: &str) -> Option<Value> {
        let thread_id = self
            .threads
            .iter()
            .find(|(_, t)| t.messages.iter().any(|(m, _, _)| m == id))?
            .0;
        let raw = format!(
            "From: Ada <ada@example.com>\r\nTo: bob@example.com\r\nSubject: Hi\r\n\
             Content-Type: text/plain; charset=utf-8\r\n\r\nBody of {id}\r\n"
        );
        Some(json!({
            "id": id,
            "threadId": thread_id,
            "raw": openmail_mime::encode_base64url(raw.as_bytes()),
        }))
    }
}

/// One embedded request from a batch: its `Content-ID` number and request target.
#[derive(Debug, Clone)]
pub struct BatchItem {
    pub id: usize,
    /// Such as `/gmail/v1/users/me/threads/t1?format=metadata&…`.
    pub target: String,
}

/// Answers batched `threads.get` and `messages.get` calls from a [`Mailbox`],
/// with `404` for unknown IDs, and records every batch.
#[derive(Clone)]
pub struct BatchResponder {
    mailbox: Arc<Mutex<Mailbox>>,
    calls: Arc<Mutex<Vec<Vec<BatchItem>>>>,
    /// IDs answered with a rate-limit 429 the first time they are requested.
    rate_limit_once: Arc<Mutex<HashSet<String>>>,
    /// A per-user quota enforced over a sliding window, if any.
    quota: Arc<Mutex<Option<QuotaWindow>>>,
}

/// Gmail's per-user quota as a sliding window: `units` per `window`, with
/// every call counted, rejected ones included.
#[derive(Debug)]
struct QuotaWindow {
    units: u32,
    window: Duration,
    spent: VecDeque<(Instant, u32)>,
    rejected: usize,
}

impl BatchResponder {
    pub fn new(mailbox: Mailbox) -> Self {
        Self {
            mailbox: Arc::new(Mutex::new(mailbox)),
            calls: Arc::default(),
            rate_limit_once: Arc::default(),
            quota: Arc::default(),
        }
    }

    /// Enforces a per-user quota of `units` per sliding `window`, counting
    /// `threads.get` as 10 units and `messages.get` as 5, and answering calls
    /// over it with a rate-limit 429 (which still count).
    pub fn enforce_quota(self, units: u32, window: Duration) -> Self {
        *self.quota.lock().unwrap() = Some(QuotaWindow {
            units,
            window,
            spent: VecDeque::new(),
            rejected: 0,
        });
        self
    }

    /// How many calls the enforced quota rejected.
    pub fn quota_rejections(&self) -> usize {
        self.quota
            .lock()
            .unwrap()
            .as_ref()
            .map_or(0, |q| q.rejected)
    }

    /// Answers these IDs with a rate-limit 429 the first time they are
    /// requested, as Gmail does to some items of a busy batch.
    pub fn rate_limit_once(self, ids: &[&str]) -> Self {
        self.rate_limit_once
            .lock()
            .unwrap()
            .extend(ids.iter().map(|id| (*id).to_owned()));
        self
    }

    /// Replaces the mailbox it serves, as if the server's mail changed.
    pub fn set_mailbox(&self, mailbox: Mailbox) {
        *self.mailbox.lock().unwrap() = mailbox;
    }

    /// Mounts the responder on the batch endpoint, and `messages.list` over
    /// the same mailbox.
    pub async fn mount(&self, server: &MockServer) {
        Mock::given(method("POST"))
            .and(path(BATCH))
            .respond_with(self.clone())
            .mount(server)
            .await;
        get("messages")
            .respond_with(MessageLister {
                mailbox: self.mailbox.clone(),
            })
            .mount(server)
            .await;
    }

    /// Every batch received so far.
    pub fn calls(&self) -> Vec<Vec<BatchItem>> {
        self.calls.lock().unwrap().clone()
    }

    /// The resource IDs requested from `collection` (`threads` or `messages`),
    /// in order.
    pub fn requested(&self, collection: &str) -> Vec<String> {
        let prefix = format!("{USER}/{collection}/");
        self.calls()
            .into_iter()
            .flatten()
            .filter_map(|item| {
                let rest = item.target.strip_prefix(&prefix)?;
                Some(rest.split('?').next().unwrap().to_owned())
            })
            .collect()
    }

    fn answer(&self, target: &str) -> (u16, Value) {
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let rest = path.strip_prefix(USER).unwrap_or(path);
        let id = rest.rsplit('/').next().unwrap_or_default();
        if let Some(quota) = self.quota.lock().unwrap().as_mut() {
            let now = Instant::now();
            while quota
                .spent
                .front()
                .is_some_and(|(at, _)| now.duration_since(*at) >= quota.window)
            {
                quota.spent.pop_front();
            }
            let cost = if rest.starts_with("/threads/") { 10 } else { 5 };
            let used: u32 = quota.spent.iter().map(|(_, units)| units).sum();
            quota.spent.push_back((now, cost));
            if used + cost > quota.units {
                quota.rejected += 1;
                return (
                    429,
                    json!({"error": {"code": 429,
                           "message": "Quota exceeded for quota metric 'Queries'",
                           "errors": [{"reason": "rateLimitExceeded"}]}}),
                );
            }
        }
        if self.rate_limit_once.lock().unwrap().remove(id) {
            return (
                429,
                json!({"error": {"code": 429, "message": "Too many concurrent requests for user",
                       "errors": [{"reason": "rateLimitExceeded"}]}}),
            );
        }
        let mailbox = self.mailbox.lock().unwrap();
        let found = if let Some(id) = rest.strip_prefix("/threads/") {
            mailbox.thread_json(id)
        } else if let Some(id) = rest.strip_prefix("/messages/") {
            if query.contains("format=raw") {
                mailbox.raw_message_json(id)
            } else {
                mailbox.metadata_message_json(id)
            }
        } else {
            None
        };
        found.map_or_else(
            || (404, json!({"error": {"code": 404, "message": "Not Found"}})),
            |body| (200, body),
        )
    }
}

impl Respond for BatchResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body = String::from_utf8(request.body.clone()).unwrap();
        let mut items = Vec::new();
        let mut current = None;
        for line in body.lines() {
            if let Some(id) = line
                .strip_prefix("Content-ID: <item-")
                .and_then(|l| l.strip_suffix('>'))
            {
                current = Some(id.parse().unwrap());
            } else if let Some(request_line) = line.strip_prefix("GET ") {
                let target = request_line.split(' ').next().unwrap().to_owned();
                items.push(BatchItem {
                    id: current.take().expect("a Content-ID before the request"),
                    target,
                });
            }
        }

        let boundary = "batch_response";
        let mut out = String::new();
        for item in &items {
            let (status, json) = self.answer(&item.target);
            out.push_str(&format!(
                "--{boundary}\r\nContent-Type: application/http\r\nContent-ID: <response-item-{}>\r\n\r\n\
                 HTTP/1.1 {status} X\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{json}\r\n",
                item.id
            ));
        }
        out.push_str(&format!("--{boundary}--\r\n"));
        self.calls.lock().unwrap().push(items);
        ResponseTemplate::new(200).set_body_raw(
            out.into_bytes(),
            &format!("multipart/mixed; boundary={boundary}"),
        )
    }
}

/// Answers `messages.list` from a [`Mailbox`]: every message, newest first,
/// after the `after:` of `q` if given, in pages of `maxResults` (100 by
/// default) whose tokens are offsets.
struct MessageLister {
    mailbox: Arc<Mutex<Mailbox>>,
}

impl Respond for MessageLister {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let params: HashMap<String, String> = request.url.query_pairs().into_owned().collect();
        let after_ms = params
            .get("q")
            .and_then(|q| q.strip_prefix("after:"))
            .map(|secs| secs.parse::<i64>().unwrap() * 1000);
        let size: usize = params.get("maxResults").map_or(100, |n| n.parse().unwrap());
        let offset: usize = params.get("pageToken").map_or(0, |t| t.parse().unwrap());
        let listing: Vec<_> = self
            .mailbox
            .lock()
            .unwrap()
            .listing()
            .into_iter()
            .filter(|(_, _, date)| after_ms.is_none_or(|after| *date > after))
            .collect();
        let page: Vec<Value> = listing
            .iter()
            .skip(offset)
            .take(size)
            .map(|(id, thread, _)| json!({"id": id, "threadId": thread}))
            .collect();
        let mut body = json!({"messages": page, "resultSizeEstimate": listing.len()});
        if offset + size < listing.len() {
            body["nextPageToken"] = json!((offset + size).to_string());
        }
        ResponseTemplate::new(200).set_body_json(body)
    }
}

/// A `history.list` mock for the page starting at `start` (with `token`).
pub fn history_list(start: u64, token: Option<&str>) -> MockBuilder {
    let mock = get("history").and(query_param("startHistoryId", start.to_string()));
    match token {
        Some(token) => mock.and(query_param("pageToken", token)),
        None => mock.and(query_param_is_missing("pageToken")),
    }
}

/// A `history.list` response.
pub fn history_page(records: Vec<Value>, history_id: u64, next: Option<&str>) -> ResponseTemplate {
    let mut body = json!({"history": records, "historyId": history_id.to_string()});
    if let Some(next) = next {
        body["nextPageToken"] = json!(next);
    }
    ResponseTemplate::new(200).set_body_json(body)
}

fn history_message(message: &str, thread: &str, labels: &[&str]) -> Value {
    json!({"id": message, "threadId": thread, "labelIds": labels})
}

/// A history record of `message` added to `thread`.
pub fn message_added(id: u64, message: &str, thread: &str, labels: &[&str]) -> Value {
    let m = history_message(message, thread, labels);
    json!({"id": id.to_string(), "messages": [m], "messagesAdded": [{"message": m}]})
}

/// A history record of `message` deleted from `thread`.
pub fn message_deleted(id: u64, message: &str, thread: &str) -> Value {
    let m = history_message(message, thread, &[]);
    json!({"id": id.to_string(), "messages": [m], "messagesDeleted": [{"message": m}]})
}

/// A history record of `labels` added to `message`.
pub fn labels_added(id: u64, message: &str, thread: &str, labels: &[&str]) -> Value {
    let m = history_message(message, thread, labels);
    json!({"id": id.to_string(), "messages": [m],
           "labelsAdded": [{"message": m, "labelIds": labels}]})
}

/// A history record of `labels` removed from `message`.
pub fn labels_removed(id: u64, message: &str, thread: &str, labels: &[&str]) -> Value {
    let m = history_message(message, thread, &[]);
    json!({"id": id.to_string(), "messages": [m],
           "labelsRemoved": [{"message": m, "labelIds": labels}]})
}
