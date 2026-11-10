//! Labels, threads and messages endpoints against `wiremock` fixtures.

mod common;

use common::{USER, json, setup};
use openmail_core::{
    HistoryId, LabelId, LabelListVisibility, LabelType, MessageId, MessageListVisibility, ThreadId,
};
use openmail_gmail::api::{Error, ListRequest, METADATA_HEADERS, MessageFormat, ThreadFormat};
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, Request};

#[tokio::test]
async fn list_labels() {
    let (server, client, _) = setup().await;
    Mock::given(method("GET"))
        .and(path(format!("{USER}/labels")))
        .respond_with(json(200, "labels_list.json"))
        .expect(1)
        .mount(&server)
        .await;

    let labels = client.list_labels().await.unwrap();
    assert_eq!(labels.len(), 5);
    assert_eq!(labels[0].id, LabelId::new("INBOX"));
    assert_eq!(labels[0].label_type, Some(LabelType::System));
    assert_eq!(
        labels[0].message_list_visibility,
        Some(MessageListVisibility::Hide)
    );
    let work = &labels[3];
    assert_eq!(work.name, "Work/Projects");
    assert_eq!(work.label_type, Some(LabelType::User));
    assert_eq!(
        work.label_list_visibility,
        Some(LabelListVisibility::LabelShowIfUnread)
    );
    assert_eq!(work.color.as_ref().unwrap().background_color, "#16a765");
    assert_eq!(work.messages_total, None);
}

#[tokio::test]
async fn get_label_includes_counts() {
    let (server, client, _) = setup().await;
    Mock::given(method("GET"))
        .and(path(format!("{USER}/labels/Label_12")))
        .respond_with(json(200, "label_get.json"))
        .expect(1)
        .mount(&server)
        .await;

    let label = client.get_label(&LabelId::new("Label_12")).await.unwrap();
    assert_eq!(label.messages_total, Some(214));
    assert_eq!(label.messages_unread, Some(3));
    assert_eq!(label.threads_total, Some(97));
    assert_eq!(label.threads_unread, Some(2));
}

#[tokio::test]
async fn get_label_not_found() {
    let (server, client, _) = setup().await;
    Mock::given(path(format!("{USER}/labels/Label_404")))
        .respond_with(json(404, "error_404.json"))
        .expect(1)
        .mount(&server)
        .await;

    let err = client
        .get_label(&LabelId::new("Label_404"))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::NotFound { .. }), "{err:?}");
}

#[tokio::test]
async fn list_threads_sends_search_parameters() {
    let (server, client, _) = setup().await;
    Mock::given(method("GET"))
        .and(path(format!("{USER}/threads")))
        .and(query_param("q", "from:ada is:unread"))
        .and(query_param("pageToken", "tok"))
        .and(query_param("maxResults", "2"))
        .respond_with(json(200, "threads_list_page1.json"))
        .expect(1)
        .mount(&server)
        .await;

    let request = ListRequest::new()
        .query("from:ada is:unread")
        .label("INBOX")
        .label("Label_12")
        .page_token("tok")
        .max_results(2);
    let page = client.list_threads(&request).await.unwrap();
    assert_eq!(page.threads.len(), 2);
    assert_eq!(page.threads[0].id, ThreadId::new("18c3a1f2b4d5e6f7"));
    assert_eq!(page.threads[0].history_id, Some(HistoryId::new(1_029_380)));
    assert_eq!(
        page.threads[0].snippet.as_deref(),
        Some("Sounds good, see you then")
    );
    assert!(page.threads[0].messages.is_empty());
    assert_eq!(
        page.next_page_token.as_deref(),
        Some("09876543210987654321")
    );
    assert_eq!(page.result_size_estimate, Some(3));

    // labelIds repeats, once per label.
    let requests = server.received_requests().await.unwrap();
    assert_eq!(label_ids(&requests[0]), ["INBOX", "Label_12"]);
}

#[tokio::test]
async fn list_threads_with_no_results() {
    let (server, client, _) = setup().await;
    Mock::given(path(format!("{USER}/threads")))
        .and(query_param_is_missing("q"))
        .and(query_param_is_missing("includeSpamTrash"))
        .respond_with(json(200, "threads_list_empty.json"))
        .expect(1)
        .mount(&server)
        .await;

    let page = client.list_threads(&ListRequest::new()).await.unwrap();
    assert!(page.threads.is_empty());
    assert_eq!(page.next_page_token, None);
}

#[tokio::test]
async fn get_thread_metadata_with_headers() {
    let (server, client, _) = setup().await;
    Mock::given(method("GET"))
        .and(path(format!("{USER}/threads/18c3a1f2b4d5e6f7")))
        .and(query_param("format", "metadata"))
        .respond_with(json(200, "thread_metadata.json"))
        .expect(1)
        .mount(&server)
        .await;

    let thread = client
        .get_thread(
            &ThreadId::new("18c3a1f2b4d5e6f7"),
            ThreadFormat::Metadata,
            METADATA_HEADERS,
        )
        .await
        .unwrap();
    assert_eq!(thread.messages.len(), 3);
    let reply = &thread.messages[1];
    assert_eq!(reply.internal_date, Some(1_699_266_600_000));
    assert_eq!(reply.size_estimate, Some(58231));
    let payload = reply.payload.as_ref().unwrap();
    assert_eq!(payload.header("subject"), Some("Re: Lunch on Friday?"));
    assert_eq!(payload.header("CC"), Some("grace@example.com"));

    let requests = server.received_requests().await.unwrap();
    let headers: Vec<_> = requests[0]
        .url
        .query_pairs()
        .filter(|(k, _)| k == "metadataHeaders")
        .map(|(_, v)| v.into_owned())
        .collect();
    assert_eq!(headers, METADATA_HEADERS);
}

#[tokio::test]
async fn get_thread_minimal_sends_no_metadata_headers() {
    let (server, client, _) = setup().await;
    Mock::given(path(format!("{USER}/threads/t1")))
        .and(query_param("format", "minimal"))
        .and(query_param_is_missing("metadataHeaders"))
        .respond_with(json(200, "thread_metadata.json"))
        .expect(1)
        .mount(&server)
        .await;

    client
        .get_thread(
            &ThreadId::new("t1"),
            ThreadFormat::Minimal,
            METADATA_HEADERS,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn list_messages() {
    let (server, client, _) = setup().await;
    Mock::given(path(format!("{USER}/messages")))
        .and(query_param("labelIds", "INBOX"))
        .and(query_param("includeSpamTrash", "true"))
        .respond_with(json(200, "messages_list.json"))
        .expect(1)
        .mount(&server)
        .await;

    let page = client
        .list_messages(&ListRequest::new().label("INBOX").include_spam_trash(true))
        .await
        .unwrap();
    assert_eq!(page.messages.len(), 2);
    assert_eq!(page.messages[0].id, MessageId::new("18c3a3bb00000003"));
    assert_eq!(
        page.messages[0].thread_id,
        ThreadId::new("18c3a1f2b4d5e6f7")
    );
    assert_eq!(page.messages[0].history_id, None);
    assert_eq!(page.next_page_token.as_deref(), Some("page-2"));
}

#[tokio::test]
async fn get_message_full_decodes_parts() {
    let (server, client, _) = setup().await;
    Mock::given(path(format!("{USER}/messages/18c3a2aa00000002")))
        .and(query_param("format", "full"))
        .respond_with(json(200, "message_full.json"))
        .expect(1)
        .mount(&server)
        .await;

    let message = client
        .get_message(
            &MessageId::new("18c3a2aa00000002"),
            MessageFormat::Full,
            &[],
        )
        .await
        .unwrap();
    let payload = message.payload.as_ref().unwrap();
    let ids: Vec<_> = payload.walk().map(|p| p.part_id.as_str()).collect();
    assert_eq!(ids, ["", "0", "0.0", "0.0.0", "0.0.1", "0.1", "1"]);

    let html = payload.walk().find(|p| p.mime_type == "text/html").unwrap();
    let body = html.body.as_ref().unwrap().decode_data().unwrap().unwrap();
    assert!(String::from_utf8(body).unwrap().contains("<b>plans</b>"));

    let pdf = payload.walk().find(|p| p.filename == "plans.pdf").unwrap();
    let body = pdf.body.as_ref().unwrap();
    assert_eq!(
        body.attachment_id.as_ref().unwrap().as_str(),
        "ANGjdJ_plans_attachment_id"
    );
    assert_eq!(body.decode_data().unwrap(), None);
}

#[tokio::test]
async fn get_message_raw() {
    let (server, client, _) = setup().await;
    Mock::given(path(format!("{USER}/messages/18c3a3bb00000003")))
        .and(query_param("format", "raw"))
        .respond_with(json(200, "message_raw.json"))
        .expect(1)
        .mount(&server)
        .await;

    let message = client
        .get_message(&MessageId::new("18c3a3bb00000003"), MessageFormat::Raw, &[])
        .await
        .unwrap();
    assert!(message.payload.is_none());
    let raw = String::from_utf8(message.decode_raw().unwrap().unwrap()).unwrap();
    assert!(raw.starts_with("From: Ada Lovelace <ada@example.com>\r\n"));
    assert!(raw.contains("\r\nSubject: Re: Lunch on Friday?\r\n"));
    assert!(raw.ends_with("\r\n\r\nSounds good, see you then?\r\n"));
}

#[tokio::test]
async fn ids_are_percent_encoded_in_paths() {
    let (server, client, _) = setup().await;
    Mock::given(path(format!("{USER}/messages/a%2Fb%3Fc")))
        .respond_with(json(200, "message_raw.json"))
        .expect(1)
        .mount(&server)
        .await;

    client
        .get_message(&MessageId::new("a/b?c"), MessageFormat::Raw, &[])
        .await
        .unwrap();
}

#[tokio::test]
async fn malformed_json_is_a_decode_error() {
    let (server, client, _) = setup().await;
    Mock::given(path(format!("{USER}/labels")))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("{\"labels\": 5}"))
        .mount(&server)
        .await;

    let err = client.list_labels().await.unwrap_err();
    assert!(matches!(err, Error::Decode(_)), "{err:?}");
}

fn label_ids(request: &Request) -> Vec<String> {
    request
        .url
        .query_pairs()
        .filter(|(k, _)| k == "labelIds")
        .map(|(_, v)| v.into_owned())
        .collect()
}
