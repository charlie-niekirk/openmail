//! `history.list` and `messages.attachments.get` against `wiremock` fixtures.

mod common;

use common::{USER, json, setup};
use openmail_core::{AttachmentId, HistoryId, LabelId, MessageId};
use openmail_gmail::api::{Error, HistoryRequest, HistoryType};
use wiremock::Mock;
use wiremock::matchers::{method, path, query_param, query_param_is_missing};

#[tokio::test]
async fn list_history_sends_its_parameters() {
    let (server, client, _) = setup().await;
    Mock::given(method("GET"))
        .and(path(format!("{USER}/history")))
        .and(query_param("startHistoryId", "1029380"))
        .and(query_param("labelId", "INBOX"))
        .and(query_param("maxResults", "100"))
        .respond_with(json(200, "history_page1.json"))
        .expect(1)
        .mount(&server)
        .await;

    let request = HistoryRequest::new(HistoryId::new(1_029_380))
        .history_types(HistoryType::ALL)
        .label("INBOX")
        .max_results(100);
    let page = client.list_history(&request).await.unwrap();

    assert_eq!(page.history_id, HistoryId::new(1_029_420));
    assert_eq!(page.next_page_token.as_deref(), Some("history-page-2"));
    assert_eq!(page.history.len(), 2);

    let added = &page.history[0];
    assert_eq!(added.id, HistoryId::new(1_029_381));
    assert_eq!(added.messages_added.len(), 1);
    assert_eq!(
        added.messages_added[0].message.id,
        MessageId::new("18c3a4cc00000004")
    );
    assert!(
        added.messages_added[0]
            .message
            .label_ids
            .contains(&LabelId::new("UNREAD"))
    );

    let read = &page.history[1];
    assert!(read.messages_added.is_empty());
    assert_eq!(read.labels_removed[0].label_ids, [LabelId::new("UNREAD")]);

    let requests = server.received_requests().await.unwrap();
    let types: Vec<_> = requests[0]
        .url
        .query_pairs()
        .filter(|(k, _)| k == "historyTypes")
        .map(|(_, v)| v.into_owned())
        .collect();
    assert_eq!(
        types,
        [
            "messageAdded",
            "messageDeleted",
            "labelAdded",
            "labelRemoved"
        ]
    );
}

#[tokio::test]
async fn list_history_with_no_changes() {
    let (server, client, _) = setup().await;
    Mock::given(path(format!("{USER}/history")))
        .and(query_param_is_missing("historyTypes"))
        .and(query_param_is_missing("labelId"))
        .respond_with(json(200, "history_empty.json"))
        .expect(1)
        .mount(&server)
        .await;

    let page = client
        .list_history(&HistoryRequest::new(HistoryId::new(1_029_420)))
        .await
        .unwrap();
    assert!(page.history.is_empty());
    assert_eq!(page.next_page_token, None);
    assert_eq!(page.history_id, HistoryId::new(1_029_420));
}

#[tokio::test]
async fn expired_history_id_is_typed() {
    let (server, client, _) = setup().await;
    Mock::given(path(format!("{USER}/history")))
        .respond_with(json(404, "error_404.json"))
        .expect(1)
        .mount(&server)
        .await;

    let err = client
        .list_history(&HistoryRequest::new(HistoryId::new(5)))
        .await
        .unwrap_err();
    match err {
        Error::HistoryExpired { start } => assert_eq!(start, HistoryId::new(5)),
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn get_attachment_decodes_its_data() {
    let (server, client, _) = setup().await;
    Mock::given(method("GET"))
        .and(path(format!(
            "{USER}/messages/18c3a2aa00000002/attachments/ANGjdJ_plans_attachment_id"
        )))
        .respond_with(json(200, "attachment.json"))
        .expect(1)
        .mount(&server)
        .await;

    let body = client
        .get_attachment(
            &MessageId::new("18c3a2aa00000002"),
            &AttachmentId::new("ANGjdJ_plans_attachment_id"),
        )
        .await
        .unwrap();
    let data = body.decode_data().unwrap().unwrap();
    assert!(data.starts_with(b"%PDF-1.4\n"));
    assert_eq!(u64::try_from(data.len()).unwrap(), body.size);
}
