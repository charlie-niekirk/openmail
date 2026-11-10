//! The batch client: request encoding, response parsing, chunking and retries.

mod common;

use common::batch::{BATCH, BatchResponder, Call, Part, error_json};
use common::{fast_retries, setup, setup_with};
use openmail_core::{MessageId, ThreadId};
use openmail_gmail::api::{Error, MAX_BATCH_SIZE, MessageFormat, ThreadFormat};
use reqwest::StatusCode;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The resource ID at the end of a part's request path.
fn resource_id(part: &Part) -> String {
    let target = part.request_line.split(' ').nth(1).unwrap();
    let path = target.split('?').next().unwrap();
    path.rsplit('/').next().unwrap().to_owned()
}

fn message_json(part: &Part) -> String {
    let id = resource_id(part);
    serde_json::json!({"id": id, "threadId": format!("t-{id}"), "labelIds": ["INBOX"]}).to_string()
}

async fn mount(server: &MockServer, responder: &BatchResponder) {
    Mock::given(method("POST"))
        .and(path(BATCH))
        .respond_with(responder.clone())
        .mount(server)
        .await;
}

fn message_ids(n: usize) -> Vec<MessageId> {
    (0..n).map(|i| MessageId::new(format!("m{i}"))).collect()
}

#[tokio::test]
async fn encodes_each_call_as_an_application_http_part() {
    let (server, client, _) = setup().await;
    let responder = BatchResponder::new(|_, part| (200, message_json(part)));
    mount(&server, &responder).await;

    client
        .get_messages(
            &[MessageId::new("a"), MessageId::new("b/c")],
            MessageFormat::Metadata,
            &["From", "Subject"],
        )
        .await
        .unwrap();

    let calls = responder.calls();
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert_eq!(call.token, "token-0");
    assert!(
        call.content_type
            .starts_with("multipart/mixed; boundary=batch_"),
        "{}",
        call.content_type
    );
    assert_eq!(call.ids(), [0, 1]);
    assert!(
        call.parts[0]
            .mime_headers
            .contains("Content-Type: application/http"),
    );
    assert_eq!(
        call.parts[0].request_line,
        "GET /gmail/v1/users/me/messages/a?format=metadata&metadataHeaders=From&metadataHeaders=Subject HTTP/1.1"
    );
    // Path segments are percent-encoded.
    assert!(
        call.parts[1]
            .request_line
            .starts_with("GET /gmail/v1/users/me/messages/b%2Fc?"),
        "{}",
        call.parts[1].request_line
    );
}

#[tokio::test]
async fn results_come_back_in_input_order_matched_by_content_id() {
    let (server, client, _) = setup().await;
    // The responder answers in reverse order.
    let responder = BatchResponder::new(|_, part| (200, message_json(part)));
    mount(&server, &responder).await;

    let ids = message_ids(5);
    let results = client
        .get_messages(&ids, MessageFormat::Minimal, &[])
        .await
        .unwrap();
    let got: Vec<_> = results.into_iter().map(|r| r.unwrap().id).collect();
    assert_eq!(got, ids);
}

#[tokio::test]
async fn get_threads_decodes_threads() {
    let (server, client, _) = setup().await;
    let responder = BatchResponder::new(|_, part| {
        let id = resource_id(part);
        let json = serde_json::json!({
            "id": id,
            "historyId": "42",
            "messages": [{"id": format!("{id}-1"), "threadId": id}]
        });
        (200, json.to_string())
    });
    mount(&server, &responder).await;

    let threads = client
        .get_threads(
            &[ThreadId::new("t1"), ThreadId::new("t2")],
            ThreadFormat::Full,
            &[],
        )
        .await
        .unwrap();
    assert_eq!(threads.len(), 2);
    let t2 = threads[1].as_ref().unwrap();
    assert_eq!(t2.id, ThreadId::new("t2"));
    assert_eq!(t2.messages[0].id, MessageId::new("t2-1"));
    assert_eq!(
        responder.calls()[0].parts[0].request_line,
        "GET /gmail/v1/users/me/threads/t1?format=full HTTP/1.1"
    );
}

#[tokio::test]
async fn mixed_success_and_failure_are_reported_per_item() {
    let (server, client, _) = setup().await;
    let responder = BatchResponder::new(|_, part| match resource_id(part).as_str() {
        "m1" => (404, error_json(404, "notFound")),
        "m2" => (400, error_json(400, "invalidArgument")),
        "m3" => (200, "not json".to_owned()),
        _ => (200, message_json(part)),
    });
    mount(&server, &responder).await;

    let results = client
        .get_messages(&message_ids(5), MessageFormat::Minimal, &[])
        .await
        .unwrap();
    assert_eq!(responder.calls().len(), 1, "4xx items are not retried");
    assert_eq!(results[0].as_ref().unwrap().id, MessageId::new("m0"));
    assert!(
        matches!(results[1], Err(Error::NotFound { .. })),
        "{:?}",
        results[1]
    );
    match &results[2] {
        Err(Error::Api { status, reason, .. }) => {
            assert_eq!(*status, StatusCode::BAD_REQUEST);
            assert_eq!(reason.as_deref(), Some("invalidArgument"));
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(
        matches!(results[3], Err(Error::Decode(_))),
        "{:?}",
        results[3]
    );
    assert!(results[4].is_ok());
}

#[tokio::test]
async fn large_inputs_are_split_into_batches_of_fifty() {
    let (server, client, _) = setup().await;
    let responder = BatchResponder::new(|_, part| (200, message_json(part)));
    mount(&server, &responder).await;

    let ids = message_ids(2 * MAX_BATCH_SIZE + 20);
    let results = client
        .get_messages(&ids, MessageFormat::Minimal, &[])
        .await
        .unwrap();

    let sizes: Vec<_> = responder.calls().iter().map(|c| c.parts.len()).collect();
    assert_eq!(sizes, [50, 50, 20]);
    assert_eq!(responder.calls()[1].ids()[0], 50);
    let got: Vec<_> = results.into_iter().map(|r| r.unwrap().id).collect();
    assert_eq!(got, ids);
}

#[tokio::test]
async fn an_empty_input_sends_nothing() {
    let (server, client, _) = setup().await;
    let responder = BatchResponder::new(|_, part| (200, message_json(part)));
    mount(&server, &responder).await;

    let results = client
        .get_messages(&[], MessageFormat::Minimal, &[])
        .await
        .unwrap();
    assert!(results.is_empty());
    assert!(responder.calls().is_empty());
}

#[tokio::test]
async fn only_failed_retryable_items_are_resent() {
    let (server, client, _) = setup().await;
    let responder = BatchResponder::new(|call: &Call, part| {
        let first = call.parts.len() == 4;
        match (first, resource_id(part).as_str()) {
            (true, "m1") => (503, error_json(503, "backendError")),
            (true, "m2") => (429, error_json(429, "rateLimitExceeded")),
            (true, "m3") => (403, error_json(403, "userRateLimitExceeded")),
            _ => (200, message_json(part)),
        }
    });
    mount(&server, &responder).await;

    let results = client
        .get_messages(&message_ids(4), MessageFormat::Minimal, &[])
        .await
        .unwrap();

    let calls = responder.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].ids(), [1, 2, 3]);
    assert!(results.iter().all(Result::is_ok), "{results:?}");
}

#[tokio::test]
async fn items_that_keep_failing_report_the_last_error() {
    let (server, client, _) = setup_with(fast_retries(2)).await;
    let responder = BatchResponder::new(|_, part| match resource_id(part).as_str() {
        "m0" => (500, error_json(500, "backendError")),
        _ => (200, message_json(part)),
    });
    mount(&server, &responder).await;

    let results = client
        .get_messages(&message_ids(2), MessageFormat::Minimal, &[])
        .await
        .unwrap();

    let sizes: Vec<_> = responder.calls().iter().map(|c| c.parts.len()).collect();
    assert_eq!(sizes, [2, 1, 1], "one try and two retries");
    match &results[0] {
        Err(Error::Api { status, .. }) => assert_eq!(*status, StatusCode::INTERNAL_SERVER_ERROR),
        other => panic!("unexpected {other:?}"),
    }
    assert!(results[1].is_ok());
}

#[tokio::test]
async fn an_item_401_refreshes_the_token_once_and_resends_it() {
    let (server, client, tokens) = setup().await;
    let responder = BatchResponder::new(|call: &Call, part| {
        match (call.token.as_str(), resource_id(part).as_str()) {
            ("token-0", "m1") => (401, error_json(401, "authError")),
            _ => (200, message_json(part)),
        }
    });
    mount(&server, &responder).await;

    let results = client
        .get_messages(&message_ids(2), MessageFormat::Minimal, &[])
        .await
        .unwrap();

    assert_eq!(tokens.refreshes(), 1);
    let calls = responder.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].token, "token-1");
    assert_eq!(calls[1].ids(), [1]);
    assert!(results.iter().all(Result::is_ok), "{results:?}");
}

#[tokio::test]
async fn a_second_item_401_is_unauthorized() {
    let (server, client, tokens) = setup().await;
    let responder = BatchResponder::new(|_, _| (401, error_json(401, "authError")));
    mount(&server, &responder).await;

    let results = client
        .get_messages(&message_ids(1), MessageFormat::Minimal, &[])
        .await
        .unwrap();

    assert_eq!(tokens.refreshes(), 1);
    assert_eq!(responder.calls().len(), 2);
    assert!(
        matches!(results[0], Err(Error::Unauthorized)),
        "{results:?}"
    );
}

#[tokio::test]
async fn a_failed_batch_request_is_retried_as_a_whole() {
    let (server, client, _) = setup().await;
    Mock::given(path(BATCH))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    let responder = BatchResponder::new(|_, part| (200, message_json(part)));
    mount(&server, &responder).await;

    let results = client
        .get_messages(&message_ids(3), MessageFormat::Minimal, &[])
        .await
        .unwrap();
    assert_eq!(responder.calls().len(), 1);
    assert!(results.iter().all(Result::is_ok));
}

#[tokio::test]
async fn a_batch_that_fails_as_a_whole_is_an_error() {
    let (server, client, _) = setup().await;
    Mock::given(path(BATCH))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_raw(error_json(400, "badRequest"), "application/json"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let err = client
        .get_messages(&message_ids(3), MessageFormat::Minimal, &[])
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Api { .. }), "{err:?}");
}

#[tokio::test]
async fn a_missing_part_is_an_invalid_response_for_that_item() {
    let (server, client, _) = setup().await;
    let body = "--b\r\nContent-Type: application/http\r\nContent-ID: <response-item-1>\r\n\r\n\
                HTTP/1.1 200 OK\r\n\r\n{\"id\":\"m1\",\"threadId\":\"t\"}\r\n--b--\r\n";
    Mock::given(path(BATCH))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "multipart/mixed; boundary=b"))
        .mount(&server)
        .await;

    let results = client
        .get_messages(&message_ids(2), MessageFormat::Minimal, &[])
        .await
        .unwrap();
    assert!(
        matches!(results[0], Err(Error::InvalidResponse(_))),
        "{results:?}"
    );
    assert_eq!(results[1].as_ref().unwrap().id, MessageId::new("m1"));
}
