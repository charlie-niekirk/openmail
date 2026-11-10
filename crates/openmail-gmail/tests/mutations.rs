//! Calls that change the mailbox: the request each one sends.

mod common;

use common::{USER, setup};
use openmail_core::{LabelId, MessageId, ThreadId};
use openmail_gmail::api::{MAX_BATCH_MODIFY_IDS, ModifyLabels};
use serde_json::json;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, ResponseTemplate};

fn archive_and_star() -> ModifyLabels {
    ModifyLabels::new()
        .add_label("STARRED")
        .remove_label("INBOX")
}

#[test]
fn modify_labels_omits_empty_lists() {
    let labels = ModifyLabels::new().remove_label(LabelId::new("UNREAD"));
    assert_eq!(
        serde_json::to_value(&labels).unwrap(),
        json!({"removeLabelIds": ["UNREAD"]})
    );
    assert!(ModifyLabels::new().is_empty());
    assert!(!labels.is_empty());
}

#[tokio::test]
async fn modify_message() {
    let (server, client, _) = setup().await;
    Mock::given(method("POST"))
        .and(path(format!("{USER}/messages/m1/modify")))
        .and(header("content-type", "application/json"))
        .and(body_json(
            json!({"addLabelIds": ["STARRED"], "removeLabelIds": ["INBOX"]}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"id": "m1", "threadId": "t1", "labelIds": ["STARRED", "IMPORTANT"]}),
        ))
        .expect(1)
        .mount(&server)
        .await;

    let message = client
        .modify_message(&MessageId::new("m1"), &archive_and_star())
        .await
        .unwrap();
    assert_eq!(message.id, MessageId::new("m1"));
    assert_eq!(
        message.label_ids,
        [LabelId::new("STARRED"), LabelId::new("IMPORTANT")]
    );
}

#[tokio::test]
async fn modify_thread() {
    let (server, client, _) = setup().await;
    Mock::given(method("POST"))
        .and(path(format!("{USER}/threads/t1/modify")))
        .and(body_json(
            json!({"addLabelIds": ["STARRED"], "removeLabelIds": ["INBOX"]}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "t1",
            "messages": [{"id": "m1", "threadId": "t1", "labelIds": ["STARRED"]}]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let thread = client
        .modify_thread(&ThreadId::new("t1"), &archive_and_star())
        .await
        .unwrap();
    assert_eq!(thread.id, ThreadId::new("t1"));
    assert_eq!(thread.messages[0].label_ids, [LabelId::new("STARRED")]);
}

#[tokio::test]
async fn batch_modify_messages() {
    let (server, client, _) = setup().await;
    Mock::given(method("POST"))
        .and(path(format!("{USER}/messages/batchModify")))
        .and(body_json(json!({
            "ids": ["m1", "m2"],
            "addLabelIds": ["STARRED"],
            "removeLabelIds": ["INBOX"]
        })))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    client
        .batch_modify_messages(
            &[MessageId::new("m1"), MessageId::new("m2")],
            &archive_and_star(),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn batch_modify_messages_is_chunked_at_a_thousand_ids() {
    let (server, client, _) = setup().await;
    Mock::given(method("POST"))
        .and(path(format!("{USER}/messages/batchModify")))
        .respond_with(ResponseTemplate::new(204))
        .expect(3)
        .mount(&server)
        .await;

    let ids: Vec<_> = (0..2 * MAX_BATCH_MODIFY_IDS + 1)
        .map(|i| MessageId::new(format!("m{i}")))
        .collect();
    client
        .batch_modify_messages(&ids, &ModifyLabels::new().remove_label("UNREAD"))
        .await
        .unwrap();

    let requests = server.received_requests().await.unwrap();
    let sent: Vec<Vec<String>> = requests
        .iter()
        .map(|r| {
            let body: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
            assert_eq!(body["removeLabelIds"], json!(["UNREAD"]));
            serde_json::from_value(body["ids"].clone()).unwrap()
        })
        .collect();
    let sizes: Vec<_> = sent.iter().map(Vec::len).collect();
    assert_eq!(sizes, [1000, 1000, 1]);
    assert_eq!(sent[1][0], "m1000");
    assert_eq!(sent[2][0], "m2000");
}

#[tokio::test]
async fn batch_modify_messages_with_no_ids_sends_nothing() {
    let (server, client, _) = setup().await;
    client
        .batch_modify_messages(&[], &archive_and_star())
        .await
        .unwrap();
    assert!(server.received_requests().await.unwrap().is_empty());
}
