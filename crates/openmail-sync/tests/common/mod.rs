//! A fake Gmail mailbox served by `wiremock`.

#![allow(dead_code)] // Each test binary uses a different subset.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use openmail_gmail::api::{GmailClient, RetryPolicy, TokenFuture, TokenProvider};
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
    let server = MockServer::start().await;
    let client = GmailClient::builder(Arc::new(StaticToken))
        .base_url(Url::parse(&server.uri()).unwrap())
        .retry_policy(RetryPolicy::none())
        .build()
        .unwrap();
    (server, client)
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

    fn thread_json(&self, id: &str) -> Option<Value> {
        let thread = self.threads.get(id)?;
        let messages: Vec<Value> = thread
            .messages
            .iter()
            .map(|(mid, labels, date)| {
                json!({
                    "id": mid,
                    "threadId": id,
                    "labelIds": labels,
                    "snippet": format!("snippet of {mid}"),
                    "historyId": thread.history_id.to_string(),
                    "internalDate": date.to_string(),
                    "sizeEstimate": 1234,
                    "payload": {
                        "mimeType": "text/plain",
                        "headers": [
                            {"name": "From", "value": "Ada Lovelace <ada@example.com>"},
                            {"name": "To", "value": "bob@example.com"},
                            {"name": "Subject", "value": format!("Subject {id}")},
                            {"name": "Message-ID", "value": format!("<{mid}@example.com>")},
                        ]
                    }
                })
            })
            .collect();
        Some(json!({
            "id": id,
            "historyId": thread.history_id.to_string(),
            "messages": messages,
        }))
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
    mailbox: Arc<Mailbox>,
    calls: Arc<Mutex<Vec<Vec<BatchItem>>>>,
}

impl BatchResponder {
    pub fn new(mailbox: Mailbox) -> Self {
        Self {
            mailbox: Arc::new(mailbox),
            calls: Arc::default(),
        }
    }

    /// Mounts the responder on the batch endpoint.
    pub async fn mount(&self, server: &MockServer) {
        Mock::given(method("POST"))
            .and(path(BATCH))
            .respond_with(self.clone())
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
        let (path, _) = target.split_once('?').unwrap_or((target, ""));
        let rest = path.strip_prefix(USER).unwrap_or(path);
        let found = if let Some(id) = rest.strip_prefix("/threads/") {
            self.mailbox.thread_json(id)
        } else if let Some(id) = rest.strip_prefix("/messages/") {
            self.mailbox.raw_message_json(id)
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
