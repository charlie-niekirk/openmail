//! Converting API fixtures into `openmail-core` domain types.

mod common;

use common::fixture;
use openmail_core::{
    Address, AttachmentId, HistoryId, Label, LabelColor, LabelId, LabelListVisibility, LabelType,
    Message, MessageId, MessageListVisibility, SystemLabel, Thread, ThreadId,
};
use openmail_gmail::api::{
    ApiLabel, ApiMessage, ApiThread, Error, ListLabelsResponse, ListThreadsResponse,
};
use time::macros::datetime;

fn parse<T: serde::de::DeserializeOwned>(name: &str) -> T {
    serde_json::from_str(&fixture(name)).unwrap()
}

#[test]
fn labels_convert() {
    let labels: Vec<Label> = parse::<ListLabelsResponse>("labels_list.json")
        .labels
        .into_iter()
        .map(Label::from)
        .collect();

    let inbox = &labels[0];
    assert_eq!(inbox.system_label(), Some(SystemLabel::Inbox));
    assert_eq!(inbox.message_list_visibility, MessageListVisibility::Hide);

    // System labels without visibilities get Gmail's defaults.
    let unread = &labels[1];
    assert!(unread.is_system());
    assert_eq!(unread.label_list_visibility, LabelListVisibility::LabelShow);
    assert_eq!(unread.message_list_visibility, MessageListVisibility::Show);
    assert!(labels[2].is_category());

    let work = &labels[3];
    assert!(work.is_user());
    assert_eq!(work.path().leaf(), "Projects");
    assert_eq!(
        work.color,
        Some(LabelColor {
            text_color: "#ffffff".to_owned(),
            background_color: "#16a765".to_owned(),
        })
    );
    assert_eq!(
        labels[4].label_list_visibility,
        LabelListVisibility::LabelHide
    );
}

#[test]
fn label_type_is_inferred_when_missing() {
    let label = |id: &str| -> Label {
        serde_json::from_value::<ApiLabel>(serde_json::json!({"id": id, "name": id}))
            .unwrap()
            .into()
    };
    assert_eq!(label("STARRED").label_type, LabelType::System);
    assert_eq!(label("Label_1").label_type, LabelType::User);
}

#[test]
fn metadata_message_converts() {
    let thread: ApiThread = parse("thread_metadata.json");
    let message = Message::try_from(&thread.messages[1]).unwrap();

    assert_eq!(message.id, MessageId::new("18c3a2aa00000002"));
    assert_eq!(message.thread_id, ThreadId::new("18c3a1f2b4d5e6f7"));
    assert_eq!(message.history_id, HistoryId::new(1_029_200));
    assert_eq!(message.internal_date, datetime!(2023-11-06 10:30:00 UTC));
    assert_eq!(message.size_estimate, 58231);
    assert!(message.is_important());
    assert!(!message.is_unread());

    let headers = &message.headers;
    assert_eq!(
        headers.from,
        [Address::named("Babbage, Charles", "charles@example.com")]
    );
    assert_eq!(
        headers.to,
        [Address::named("Ada Lovelace", "ada@example.com")]
    );
    assert_eq!(headers.cc, [Address::new("grace@example.com")]);
    assert!(headers.bcc.is_empty());
    assert_eq!(
        headers.reply_to,
        [Address::named("Charles", "c.babbage@example.org")]
    );
    assert_eq!(headers.subject.as_deref(), Some("Re: Lunch on Friday?"));
    assert_eq!(
        headers.message_id_header.as_deref(),
        Some("<CAB2@mail.example.com>")
    );
    assert_eq!(
        headers.in_reply_to.as_deref(),
        Some("<CAA1@mail.example.com>")
    );

    // metadata has no parts; multipart/mixed is taken to mean attachments.
    assert!(message.has_attachments);
    assert!(
        !Message::try_from(&thread.messages[0])
            .unwrap()
            .has_attachments
    );
}

#[test]
fn full_message_lists_attachments() {
    let api: ApiMessage = parse("message_full.json");
    let attachments = api.attachments();
    assert_eq!(attachments.len(), 2);

    let logo = &attachments[0];
    assert_eq!(logo.message_id, MessageId::new("18c3a2aa00000002"));
    assert_eq!(logo.part_id, "0.1");
    assert_eq!(logo.filename, "logo.png");
    assert_eq!(logo.mime_type, "image/png");
    assert_eq!(logo.content_id.as_deref(), Some("logo@example.com"));
    assert!(logo.is_inline);
    assert_eq!(logo.size, 2311);

    let pdf = &attachments[1];
    assert_eq!(pdf.part_id, "1");
    assert_eq!(
        pdf.attachment_id,
        Some(AttachmentId::new("ANGjdJ_plans_attachment_id"))
    );
    assert!(!pdf.is_inline);
    assert_eq!(pdf.content_id, None);

    let message = Message::try_from(api).unwrap();
    assert!(message.has_attachments);
    assert_eq!(message.headers.cc, [Address::new("grace@example.com")]);
}

#[test]
fn messages_without_minimal_fields_do_not_convert() {
    let list: ListThreadsResponse = parse("threads_list_page1.json");
    assert!(list.threads[0].messages.is_empty());

    let id_only: ApiMessage =
        serde_json::from_value(serde_json::json!({"id": "m1", "threadId": "t1"})).unwrap();
    let err = Message::try_from(&id_only).unwrap_err();
    assert!(matches!(err, Error::InvalidResponse(_)), "{err:?}");
    assert!(id_only.headers().from.is_empty());
    assert!(id_only.attachments().is_empty());
}

#[test]
fn thread_summarises_its_messages() {
    let api: ApiThread = parse("thread_metadata.json");
    let (thread, messages) = api.into_domain().unwrap();

    assert_eq!(messages.len(), 3);
    assert_eq!(thread.id, ThreadId::new("18c3a1f2b4d5e6f7"));
    assert_eq!(thread.history_id, HistoryId::new(1_029_380));
    assert_eq!(thread.subject.as_deref(), Some("Lunch on Friday?"));
    assert_eq!(thread.snippet, "Sounds good, see you then");
    assert_eq!(thread.last_message_date, datetime!(2023-11-06 11:15:00 UTC));
    assert_eq!(thread.message_count, 3);
    assert_eq!(
        thread.label_ids,
        [
            "SENT",
            "INBOX",
            "CATEGORY_PERSONAL",
            "IMPORTANT",
            "UNREAD",
            "Label_12"
        ]
        .map(LabelId::new)
    );
    // Ada sent twice; senders are unique, oldest first.
    assert_eq!(
        thread.senders,
        [
            Address::named("Ada Lovelace", "ada@example.com"),
            Address::named("Babbage, Charles", "charles@example.com"),
        ]
    );
    assert!(thread.is_unread());
    assert!(thread.has_attachments);
}

#[test]
fn thread_without_messages_does_not_convert() {
    let list: ListThreadsResponse = parse("threads_list_page1.json");
    let err = Thread::try_from(list.threads[0].clone()).unwrap_err();
    assert!(matches!(err, Error::InvalidResponse(_)), "{err:?}");
}
