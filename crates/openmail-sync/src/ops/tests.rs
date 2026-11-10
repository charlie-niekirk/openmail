//! The queue's local bookkeeping, without a Gmail server.

use std::collections::HashSet;

use openmail_core::{HistoryId, Message, MessageHeaders, Thread};
use openmail_gmail::api::{TokenFuture, TokenProvider};
use serde_json::json;

use super::*;

struct NoToken;

impl TokenProvider for NoToken {
    fn access_token(&self) -> TokenFuture<'_> {
        Box::pin(async { Ok("token".to_owned()) })
    }

    fn refresh_access_token<'a>(&'a self, _rejected: &'a str) -> TokenFuture<'a> {
        Box::pin(async { Ok("token".to_owned()) })
    }
}

fn queue() -> OpsQueue {
    let client = GmailClient::builder(Arc::new(NoToken))
        .base_url("http://127.0.0.1:1/".parse().unwrap())
        .build()
        .unwrap();
    OpsQueue::new(
        client,
        OpsConfig {
            undo_window: Duration::ZERO,
            ..OpsConfig::default()
        },
    )
}

/// Writes t1 with m1 carrying `labels`, as a sync would.
fn write_thread(store: &Store, labels: &[&str]) {
    let date = OffsetDateTime::from_unix_timestamp(1_000).unwrap();
    let thread = Thread {
        id: ThreadId::new("t1"),
        history_id: HistoryId::new(1),
        subject: None,
        snippet: String::new(),
        last_message_date: date,
        message_count: 1,
        label_ids: Vec::new(),
        senders: Vec::new(),
        has_attachments: false,
    };
    let message = Message {
        id: MessageId::new("m1"),
        thread_id: thread.id.clone(),
        history_id: HistoryId::new(1),
        label_ids: labels.iter().map(|l| LabelId::new(*l)).collect(),
        snippet: String::new(),
        internal_date: date,
        size_estimate: 1,
        headers: MessageHeaders::default(),
        has_attachments: false,
    };
    store.replace_threads(&[(thread, vec![message])]).unwrap();
}

fn labels(store: &Store) -> Vec<String> {
    store
        .message_label_ids(&MessageId::new("m1"))
        .unwrap()
        .into_iter()
        .map(LabelId::into_string)
        .collect()
}

#[test]
fn ops_persist_as_tagged_json() {
    let op = Op::archive(Target::thread("t1"));
    assert_eq!(
        serde_json::to_value(&op).unwrap(),
        json!({"type": "modify_labels", "target": {"threads": ["t1"]}, "remove": ["INBOX"]})
    );
    let op = Op::RenameLabel {
        label: LabelId::new("Label_1"),
        name: "Work/Old".to_owned(),
    };
    let text = serde_json::to_string(&op).unwrap();
    assert_eq!(serde_json::from_str::<Op>(&text).unwrap(), op);
    assert_eq!(op.kind(), "rename_label");
}

#[test]
fn backoff_doubles_up_to_the_cap_and_honours_retry_after() {
    let config = OpsConfig::default();
    let secs = |attempts, after: Option<u64>| {
        config
            .backoff(attempts, after.map(Duration::from_secs))
            .as_secs()
    };
    assert_eq!(secs(1, None), 2);
    assert_eq!(secs(2, None), 4);
    assert_eq!(secs(5, None), 32);
    assert_eq!(secs(30, None), 300);
    assert_eq!(secs(1, Some(30)), 30);
    assert_eq!(secs(8, Some(30)), 256);
}

#[test]
fn ops_sent_after_a_fetch_are_reapplied_over_it_and_older_ones_are_not() {
    let store = Store::open_in_memory().unwrap();
    write_thread(&store, &["INBOX", "UNREAD"]);
    let queue = queue();
    let id = queue
        .enqueue(&store, Op::archive(Target::thread("t1")))
        .unwrap();
    assert_eq!(labels(&store), ["UNREAD"]);
    let sent_at = OffsetDateTime::now_utc();
    store.claim_pending_op(id.get()).unwrap();
    store.mark_pending_op_sent(id.get(), sent_at).unwrap();
    let t1 = ThreadId::new("t1");
    let threads: HashSet<&ThreadId> = [&t1].into_iter().collect();

    // A refetch requested before the archive was sent shows t1 still in
    // the Inbox: the archive goes back on top.
    write_thread(&store, &["INBOX", "UNREAD"]);
    let before = sent_at - Duration::from_secs(1);
    assert_eq!(rebase(&store, &threads, false, before).unwrap(), 1);
    assert_eq!(labels(&store), ["UNREAD"]);

    // One requested after it is Gmail's word.
    write_thread(&store, &["INBOX"]);
    let after = sent_at + Duration::from_secs(1);
    assert_eq!(rebase(&store, &threads, false, after).unwrap(), 0);
    assert_eq!(labels(&store), ["INBOX"]);
    assert_eq!(forget_sent(&store, before).unwrap(), 0);
    assert_eq!(forget_sent(&store, after).unwrap(), 1);
}

#[test]
fn label_edits_are_reapplied_after_a_label_sync() {
    let store = Store::open_in_memory().unwrap();
    let label = openmail_core::Label {
        id: LabelId::new("Label_1"),
        name: "Work".to_owned(),
        label_type: openmail_core::LabelType::User,
        color: None,
        label_list_visibility: openmail_core::LabelListVisibility::LabelShow,
        message_list_visibility: openmail_core::MessageListVisibility::Show,
    };
    store.upsert_label(&label).unwrap();
    let queue = OpsQueue {
        config: OpsConfig::default(),
        ..queue()
    };
    let rename = queue
        .enqueue(
            &store,
            Op::RenameLabel {
                label: label.id.clone(),
                name: "Job".to_owned(),
            },
        )
        .unwrap();

    // A label sync brings back Gmail's name; the pending rename wins locally.
    store.replace_labels(std::slice::from_ref(&label)).unwrap();
    assert_eq!(
        rebase(&store, &HashSet::new(), true, OffsetDateTime::now_utc()).unwrap(),
        1
    );
    assert_eq!(store.label(&label.id).unwrap().unwrap().name, "Job");

    // Undo restores the name from before the rename.
    assert!(queue.undo(&store, rename).unwrap());
    assert_eq!(store.label(&label.id).unwrap().unwrap().name, "Work");
}
