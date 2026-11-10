//! Following page tokens with the pagination streams.

mod common;

use common::{USER, json, setup};
use futures_util::{StreamExt, TryStreamExt};
use openmail_core::{HistoryId, ThreadId};
use openmail_gmail::api::{Error, HistoryRequest, ListRequest};
use wiremock::Mock;
use wiremock::matchers::{path, query_param, query_param_is_missing};

#[tokio::test]
async fn thread_pages_follow_next_page_tokens() {
    let (server, client, _) = setup().await;
    Mock::given(path(format!("{USER}/threads")))
        .and(query_param("labelIds", "INBOX"))
        .and(query_param_is_missing("pageToken"))
        .respond_with(json(200, "threads_list_page1.json"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path(format!("{USER}/threads")))
        .and(query_param("labelIds", "INBOX"))
        .and(query_param("pageToken", "09876543210987654321"))
        .respond_with(json(200, "threads_list_page2.json"))
        .expect(1)
        .mount(&server)
        .await;

    let pages: Vec<_> = client
        .thread_pages(ListRequest::new().label("INBOX"))
        .try_collect()
        .await
        .unwrap();
    let ids: Vec<_> = pages
        .iter()
        .flat_map(|p| &p.threads)
        .map(|t| t.id.clone())
        .collect();
    assert_eq!(
        ids,
        [
            ThreadId::new("18c3a1f2b4d5e6f7"),
            ThreadId::new("18c39e0a1b2c3d4e"),
            ThreadId::new("18c2ffee00112233"),
        ]
    );
}

#[tokio::test]
async fn pages_are_fetched_lazily() {
    let (server, client, _) = setup().await;
    Mock::given(path(format!("{USER}/messages")))
        .and(query_param_is_missing("pageToken"))
        .respond_with(json(200, "messages_list.json"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path(format!("{USER}/messages")))
        .and(query_param("pageToken", "page-2"))
        .respond_with(json(200, "messages_list_page2.json"))
        .expect(0)
        .mount(&server)
        .await;

    let first: Vec<_> = client
        .message_pages(ListRequest::new())
        .take(1)
        .try_collect()
        .await
        .unwrap();
    assert_eq!(first[0].messages.len(), 2);
}

#[tokio::test]
async fn message_pages_start_from_the_given_token() {
    let (server, client, _) = setup().await;
    Mock::given(path(format!("{USER}/messages")))
        .and(query_param("pageToken", "page-2"))
        .respond_with(json(200, "messages_list_page2.json"))
        .expect(1)
        .mount(&server)
        .await;

    let pages: Vec<_> = client
        .message_pages(ListRequest::new().page_token("page-2"))
        .try_collect()
        .await
        .unwrap();
    assert_eq!(pages.len(), 1);
}

#[tokio::test]
async fn history_pages_end_with_the_current_history_id() {
    let (server, client, _) = setup().await;
    Mock::given(path(format!("{USER}/history")))
        .and(query_param("startHistoryId", "1029380"))
        .and(query_param_is_missing("pageToken"))
        .respond_with(json(200, "history_page1.json"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path(format!("{USER}/history")))
        .and(query_param("startHistoryId", "1029380"))
        .and(query_param("pageToken", "history-page-2"))
        .respond_with(json(200, "history_page2.json"))
        .expect(1)
        .mount(&server)
        .await;

    let pages: Vec<_> = client
        .history_pages(HistoryRequest::new(HistoryId::new(1_029_380)))
        .try_collect()
        .await
        .unwrap();
    assert_eq!(pages.len(), 2);
    let records: Vec<_> = pages
        .iter()
        .flat_map(|p| &p.history)
        .map(|h| h.id)
        .collect();
    assert_eq!(
        records,
        [1_029_381, 1_029_390, 1_029_401, 1_029_415].map(HistoryId::new)
    );
    assert_eq!(pages[1].history_id, HistoryId::new(1_029_420));
    assert_eq!(pages[1].history[1].messages_deleted.len(), 1);
}

#[tokio::test]
async fn a_failing_page_ends_the_stream_with_its_error() {
    let (server, client, _) = setup().await;
    Mock::given(path(format!("{USER}/history")))
        .and(query_param_is_missing("pageToken"))
        .respond_with(json(200, "history_page1.json"))
        .mount(&server)
        .await;
    Mock::given(path(format!("{USER}/history")))
        .and(query_param("pageToken", "history-page-2"))
        .respond_with(json(404, "error_404.json"))
        .mount(&server)
        .await;

    let results: Vec<_> = client
        .history_pages(HistoryRequest::new(HistoryId::new(1_029_380)))
        .collect()
        .await;
    assert_eq!(results.len(), 2);
    assert!(results[0].is_ok());
    assert!(matches!(results[1], Err(Error::HistoryExpired { .. })));
}

#[test]
fn page_streams_are_send() {
    fn assert_send<T: Send>(_: &T) {}
    let tokens = std::sync::Arc::new(common::TestTokens::default());
    let client = openmail_gmail::GmailClient::builder(tokens)
        .base_url("http://127.0.0.1:9/".parse().unwrap())
        .build()
        .unwrap();
    assert_send(&client.thread_pages(ListRequest::new()));
    assert_send(&client.history_pages(HistoryRequest::new(HistoryId::new(1))));
}
