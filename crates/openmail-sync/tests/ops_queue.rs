//! The offline ops queue against a `wiremock` Gmail and a synced store.

mod common;

use std::time::Duration;

use common::{
    BatchResponder, Listing, Mailbox, USER, list_page, mount_labels, mount_profile, page, setup,
};
use openmail_core::{LabelId, MessageId, ThreadId};
use openmail_gmail::GmailClient;
use openmail_store::{PendingOpState, Store, ThreadView};
use openmail_sync::{
    CancellationToken, Cutoff, InitialSync, InitialSyncConfig, Op, OpEvent, OpsConfig, OpsQueue,
    Target,
};
use serde_json::{Value, json};
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// t1 (unread) and t2 in the Inbox, t3 starred and archived.
fn mailbox() -> Mailbox {
    Mailbox::default()
        .with(1, &["INBOX", "UNREAD"])
        .with(2, &["INBOX"])
        .with(3, &["STARRED"])
}

/// A mock Gmail serving `mailbox` and a store fully synced from it, at
/// history ID [`PROFILE_HISTORY_ID`].
async fn synced_with(mailbox: Mailbox) -> (MockServer, Store, GmailClient, BatchResponder) {
    let (server, client) = setup().await;
    mount_profile(&server, 1).await;
    mount_labels(&server).await;
    let all: Vec<&str> = {
        let mut ids: Vec<&str> = mailbox.threads.keys().map(String::as_str).collect();
        ids.sort_unstable();
        ids
    };
    let inbox: Vec<&str> = all
        .iter()
        .copied()
        .filter(|id| {
            mailbox.threads[*id]
                .messages
                .iter()
                .any(|(_, labels, _)| labels.iter().any(|l| l == "INBOX"))
        })
        .collect();
    list_page(Listing::Inbox, None)
        .respond_with(page(&mailbox, &inbox, None))
        .mount(&server)
        .await;
    list_page(Listing::Backfill, None)
        .respond_with(page(&mailbox, &all, None))
        .mount(&server)
        .await;
    let batch = BatchResponder::new(mailbox);
    batch.mount(&server).await;
    let config = InitialSyncConfig {
        cutoff: Cutoff::All,
        ..InitialSyncConfig::default()
    };
    let mut sync = InitialSync::new(client.clone(), Store::open_in_memory().unwrap(), config);
    sync.run(&CancellationToken::new(), |_| {}).await.unwrap();
    server.reset().await;
    batch.mount(&server).await;
    (server, sync.into_store(), client, batch)
}

async fn synced() -> (MockServer, Store, GmailClient, BatchResponder) {
    synced_with(mailbox()).await
}

/// A queue that sends at once and retries at once.
fn instant_queue(client: &GmailClient) -> OpsQueue {
    OpsQueue::new(
        client.clone(),
        OpsConfig {
            undo_window: Duration::ZERO,
            initial_backoff: Duration::ZERO,
            ..OpsConfig::default()
        },
    )
}

fn thread(id: &str) -> Target {
    Target::thread(ThreadId::new(id))
}

/// The threads of a label view, by ID.
fn view(store: &Store, label: &str) -> Vec<String> {
    let mut ids: Vec<String> = store
        .thread_page(&ThreadView::Label(LabelId::new(label)), None, 100)
        .unwrap()
        .threads
        .into_iter()
        .map(|t| t.id.into_string())
        .collect();
    ids.sort();
    ids
}

fn labels_of(store: &Store, message: &str) -> Vec<String> {
    store
        .message_label_ids(&MessageId::new(message))
        .unwrap()
        .into_iter()
        .map(LabelId::into_string)
        .collect()
}

/// Flushes once, collecting the events.
async fn flush_once(queue: &OpsQueue, store: &mut Store) -> (openmail_sync::Flush, Vec<OpEvent>) {
    let mut events = Vec::new();
    let flush = queue
        .flush(store, &CancellationToken::new(), &mut |e| events.push(e))
        .await
        .unwrap();
    (flush, events)
}

fn batch_modify(body: Value) -> wiremock::MockBuilder {
    Mock::given(method("POST"))
        .and(path(format!("{USER}/messages/batchModify")))
        .and(body_json(body))
}

/// The bodies of the `batchModify` calls Gmail received.
async fn batch_modify_calls(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path().ends_with("/messages/batchModify"))
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

fn api_error(status: u16, reason: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(json!({
        "error": {"code": status, "message": reason, "errors": [{"reason": reason}]}
    }))
}

#[tokio::test]
async fn enqueue_applies_at_once_and_flush_sends_it() {
    let (server, mut store, client, _) = synced().await;
    let queue = instant_queue(&client);
    assert_eq!(view(&store, "INBOX"), ["t1", "t2"]);

    let id = queue.enqueue(&store, Op::archive(thread("t1"))).unwrap();
    queue.enqueue(&store, Op::star(thread("t1"))).unwrap();
    // The store's queries show the change before anything is sent.
    assert_eq!(view(&store, "INBOX"), ["t2"]);
    assert_eq!(view(&store, "STARRED"), ["t1", "t3"]);
    let counts = store.label_counts().unwrap();
    let inbox = counts
        .iter()
        .find(|c| c.label_id.as_str() == "INBOX")
        .unwrap();
    assert_eq!((inbox.threads_total, inbox.threads_unread), (1, 0));
    assert!(server.received_requests().await.unwrap().is_empty());

    batch_modify(json!({"ids": ["m1"], "removeLabelIds": ["INBOX"]}))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    batch_modify(json!({"ids": ["m1"], "addLabelIds": ["STARRED"]}))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    let (flush, events) = flush_once(&queue, &mut store).await;
    assert_eq!((flush.sent, flush.waiting), (2, 0));
    assert!(matches!(&events[0], OpEvent::Sent { id: sent, .. } if *sent == id));

    // Sent ops are kept until history sync has caught up with them.
    let queued = queue.queued(&store).unwrap();
    assert!(queued.iter().all(|op| op.state == PendingOpState::Sent));
    assert_eq!(view(&store, "INBOX"), ["t2"]);
}

#[tokio::test]
async fn label_changes_are_coalesced_into_batch_modify_calls() {
    let (server, mut store, client, _) = synced().await;
    let queue = instant_queue(&client);
    queue.enqueue(&store, Op::archive(thread("t1"))).unwrap();
    queue.enqueue(&store, Op::archive(thread("t2"))).unwrap();
    queue.enqueue(&store, Op::mark_read(thread("t1"))).unwrap();
    queue.enqueue(&store, Op::mark_read(thread("t2"))).unwrap();
    Mock::given(method("POST"))
        .and(path(format!("{USER}/messages/batchModify")))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    let (flush, _) = flush_once(&queue, &mut store).await;
    assert_eq!(flush.sent, 4);
    assert_eq!(
        batch_modify_calls(&server).await,
        [
            json!({"ids": ["m1", "m2"], "removeLabelIds": ["INBOX"]}),
            json!({"ids": ["m1", "m2"], "removeLabelIds": ["UNREAD"]}),
        ]
    );
}

#[tokio::test]
async fn a_change_and_its_inverse_cancel_out() {
    let (server, mut store, client, _) = synced().await;
    let queue = instant_queue(&client);
    queue.enqueue(&store, Op::star(thread("t1"))).unwrap();
    queue.enqueue(&store, Op::unstar(thread("t1"))).unwrap();
    assert_eq!(view(&store, "STARRED"), ["t3"]);

    let (flush, events) = flush_once(&queue, &mut store).await;
    assert_eq!((flush.cancelled_out, flush.sent), (2, 0));
    assert!(
        events
            .iter()
            .all(|e| matches!(e, OpEvent::CancelledOut { .. }))
    );
    assert!(server.received_requests().await.unwrap().is_empty());
    assert!(queue.queued(&store).unwrap().is_empty());
    assert_eq!(view(&store, "STARRED"), ["t3"]);
}

#[tokio::test]
async fn an_op_is_held_while_its_inverse_is_in_the_undo_window() {
    let (server, store, client, _) = synced().await;
    let queue = instant_queue(&client);
    queue.enqueue(&store, Op::star(thread("t1"))).unwrap();
    // Queued later with a long undo window.
    let slow = OpsQueue::new(
        client.clone(),
        OpsConfig {
            undo_window: Duration::from_secs(3600),
            ..OpsConfig::default()
        },
    );
    let unstar = slow.enqueue(&store, Op::unstar(thread("t1"))).unwrap();
    let mut store = store;
    let (flush, _) = flush_once(&queue, &mut store).await;
    assert_eq!((flush.sent, flush.waiting), (0, 2));
    assert!(server.received_requests().await.unwrap().is_empty());

    // Undoing the unstar lets the star go.
    assert!(slow.undo(&store, unstar).unwrap());
    assert_eq!(view(&store, "STARRED"), ["t1", "t3"]);
    batch_modify(json!({"ids": ["m1"], "addLabelIds": ["STARRED"]}))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    let (flush, _) = flush_once(&queue, &mut store).await;
    assert_eq!(flush.sent, 1);
}

#[tokio::test]
async fn server_errors_and_rate_limits_are_retried() {
    let (server, mut store, client, _) = synced().await;
    let queue = instant_queue(&client);
    queue.enqueue(&store, Op::archive(thread("t1"))).unwrap();
    batch_modify(json!({"ids": ["m1"], "removeLabelIds": ["INBOX"]}))
        .respond_with(api_error(503, "backendError"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    batch_modify(json!({"ids": ["m1"], "removeLabelIds": ["INBOX"]}))
        .respond_with(api_error(429, "rateLimitExceeded"))
        .up_to_n_times(1)
        .with_priority(2)
        .mount(&server)
        .await;
    batch_modify(json!({"ids": ["m1"], "removeLabelIds": ["INBOX"]}))
        .respond_with(ResponseTemplate::new(204))
        .with_priority(3)
        .mount(&server)
        .await;

    let (first, events) = flush_once(&queue, &mut store).await;
    assert_eq!((first.retrying, first.waiting), (1, 1));
    assert!(matches!(&events[0], OpEvent::Retrying { attempts: 1, .. }));
    let queued = queue.queued(&store).unwrap();
    assert_eq!(queued[0].attempts, 1);
    assert!(queued[0].last_error.as_deref().unwrap().contains("503"));
    // Still archived locally while it waits.
    assert_eq!(view(&store, "INBOX"), ["t2"]);

    let (second, events) = flush_once(&queue, &mut store).await;
    assert_eq!(second.retrying, 1);
    assert!(matches!(&events[0], OpEvent::Retrying { attempts: 2, .. }));

    let (third, _) = flush_once(&queue, &mut store).await;
    assert_eq!(third.sent, 1);
    assert_eq!(view(&store, "INBOX"), ["t2"]);
}

#[tokio::test]
async fn retries_back_off_and_honour_retry_after() {
    let (server, mut store, client, _) = synced().await;
    let queue = OpsQueue::new(
        client.clone(),
        OpsConfig {
            undo_window: Duration::ZERO,
            ..OpsConfig::default()
        },
    );
    queue.enqueue(&store, Op::archive(thread("t1"))).unwrap();
    queue.enqueue(&store, Op::star(thread("t1"))).unwrap();
    queue.enqueue(&store, Op::star(thread("t2"))).unwrap();
    batch_modify(json!({"ids": ["m1"], "removeLabelIds": ["INBOX"]}))
        .respond_with(api_error(429, "rateLimitExceeded").insert_header("Retry-After", "30"))
        .mount(&server)
        .await;
    batch_modify(json!({"ids": ["m2"], "addLabelIds": ["STARRED"]}))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let before = time::OffsetDateTime::now_utc();
    let (flush, events) = flush_once(&queue, &mut store).await;
    let OpEvent::Retrying { retry_at, .. } = &events[0] else {
        panic!("expected a retry: {events:?}");
    };
    assert!(*retry_at >= before + Duration::from_secs(30));
    // t1's star waits behind its archive; t2's goes.
    assert_eq!((flush.retrying, flush.sent, flush.waiting), (1, 1, 2));
    assert_eq!(flush.next_due, Some(*retry_at));
    assert_eq!(batch_modify_calls(&server).await.len(), 2);

    // Nothing is due yet.
    let (flush, _) = flush_once(&queue, &mut store).await;
    assert_eq!((flush.sent, flush.retrying, flush.waiting), (0, 0, 2));
    assert_eq!(batch_modify_calls(&server).await.len(), 2);
}

#[tokio::test]
async fn rejected_ops_are_dropped_and_rolled_back() {
    let (server, mut store, client, _) = synced().await;
    let queue = instant_queue(&client);
    queue.enqueue(&store, Op::archive(thread("t1"))).unwrap();
    queue.enqueue(&store, Op::archive(thread("t2"))).unwrap();
    queue.enqueue(&store, Op::mark_read(thread("t1"))).unwrap();
    assert!(view(&store, "INBOX").is_empty());
    // Together they fail; alone, t1 goes through and t2 is rejected.
    batch_modify(json!({"ids": ["m1", "m2"], "removeLabelIds": ["INBOX"]}))
        .respond_with(api_error(400, "invalidArgument"))
        .mount(&server)
        .await;
    batch_modify(json!({"ids": ["m1"], "removeLabelIds": ["INBOX"]}))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    batch_modify(json!({"ids": ["m2"], "removeLabelIds": ["INBOX"]}))
        .respond_with(api_error(400, "invalidArgument"))
        .mount(&server)
        .await;
    batch_modify(json!({"ids": ["m1"], "removeLabelIds": ["UNREAD"]}))
        .respond_with(api_error(403, "insufficientPermissions"))
        .mount(&server)
        .await;

    let (flush, events) = flush_once(&queue, &mut store).await;
    assert_eq!((flush.sent, flush.failed, flush.waiting), (1, 2, 0));
    let failed: Vec<&Op> = events
        .iter()
        .filter_map(|e| match e {
            OpEvent::Failed { op, .. } => Some(op),
            _ => None,
        })
        .collect();
    assert_eq!(
        failed,
        [&Op::archive(thread("t2")), &Op::mark_read(thread("t1"))]
    );
    // t2's archive and t1's mark-read are rolled back.
    assert_eq!(view(&store, "INBOX"), ["t2"]);
    assert_eq!(labels_of(&store, "m1"), ["UNREAD"]);
    assert_eq!(queue.queued(&store).unwrap().len(), 1);
}

#[tokio::test]
async fn auth_failures_stop_the_flush_and_keep_the_queue() {
    let (server, mut store, client, _) = synced().await;
    let queue = instant_queue(&client);
    queue.enqueue(&store, Op::archive(thread("t1"))).unwrap();
    batch_modify(json!({"ids": ["m1"], "removeLabelIds": ["INBOX"]}))
        .respond_with(api_error(401, "authError"))
        .mount(&server)
        .await;
    let result = queue
        .flush(&mut store, &CancellationToken::new(), &mut |_| {})
        .await;
    assert!(matches!(
        result,
        Err(openmail_sync::Error::Gmail(
            openmail_gmail::api::Error::Unauthorized
        ))
    ));
    let queued = queue.queued(&store).unwrap();
    assert_eq!(
        (queued.len(), queued[0].state, queued[0].attempts),
        (1, PendingOpState::Queued, 0)
    );
    assert_eq!(view(&store, "INBOX"), ["t2"]);
}

#[tokio::test]
async fn undo_within_the_window_reverts_and_sends_nothing() {
    let (server, mut store, client, _) = synced().await;
    let queue = OpsQueue::new(client.clone(), OpsConfig::default());
    assert_eq!(queue.config().undo_window, Duration::from_secs(5));
    let archive = queue.enqueue(&store, Op::archive(thread("t1"))).unwrap();
    let delete = queue
        .enqueue(
            &store,
            Op::Delete {
                target: thread("t2"),
            },
        )
        .unwrap();
    assert!(view(&store, "INBOX").is_empty());
    assert!(store.thread(&ThreadId::new("t2")).unwrap().is_none());

    // Not due during the window.
    let (flush, _) = flush_once(&queue, &mut store).await;
    assert_eq!((flush.sent, flush.waiting), (0, 2));
    assert!(flush.next_due.is_some());

    assert!(queue.undo(&store, archive).unwrap());
    assert!(queue.undo(&store, delete).unwrap());
    assert!(!queue.undo(&store, delete).unwrap(), "already undone");
    assert_eq!(view(&store, "INBOX"), ["t1", "t2"]);
    assert_eq!(labels_of(&store, "m2"), ["INBOX"]);
    assert!(queue.queued(&store).unwrap().is_empty());
    assert!(server.received_requests().await.unwrap().is_empty());

    // Past the window, an op can no longer be undone.
    let quick = instant_queue(&client);
    let late = quick.enqueue(&store, Op::star(thread("t2"))).unwrap();
    assert!(!quick.undo(&store, late).unwrap());
    assert_eq!(view(&store, "STARRED"), ["t2", "t3"]);
}

#[tokio::test]
async fn trash_and_delete_are_sent_per_thread() {
    let (server, mut store, client, _) = synced().await;
    let queue = instant_queue(&client);
    queue
        .enqueue(
            &store,
            Op::Trash {
                target: thread("t1"),
            },
        )
        .unwrap();
    queue
        .enqueue(
            &store,
            Op::Delete {
                target: thread("t2"),
            },
        )
        .unwrap();
    assert_eq!(labels_of(&store, "m1"), ["TRASH", "UNREAD"]);
    assert!(store.thread(&ThreadId::new("t2")).unwrap().is_none());

    Mock::given(method("POST"))
        .and(path(format!("{USER}/threads/t1/trash")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "t1"})))
        .expect(1)
        .mount(&server)
        .await;
    // Already gone counts as deleted.
    Mock::given(method("DELETE"))
        .and(path(format!("{USER}/threads/t2")))
        .respond_with(api_error(404, "notFound"))
        .expect(1)
        .mount(&server)
        .await;
    let (flush, _) = flush_once(&queue, &mut store).await;
    assert_eq!(flush.sent, 2);
}

#[tokio::test]
async fn a_label_created_offline_can_be_applied_before_it_exists() {
    let (server, mut store, client, _) = synced().await;
    let queue = instant_queue(&client);
    let create = queue
        .enqueue(
            &store,
            Op::CreateLabel {
                name: "Receipts".to_owned(),
                color: None,
            },
        )
        .unwrap();
    let local = queue.local_label(&store, create).unwrap().unwrap();
    queue
        .enqueue(
            &store,
            Op::modify(thread("t2"), [local.clone()], [LabelId::new("INBOX")]),
        )
        .unwrap();
    assert_eq!(view(&store, local.as_str()), ["t2"]);

    Mock::given(method("POST"))
        .and(path(format!("{USER}/labels")))
        .and(body_json(json!({"name": "Receipts"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "Label_9", "name": "Receipts", "type": "user"
        })))
        .expect(1)
        .mount(&server)
        .await;
    batch_modify(json!({"ids": ["m2"], "addLabelIds": ["Label_9"], "removeLabelIds": ["INBOX"]}))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    let mut sent = 0;
    queue
        .drain(&mut store, &CancellationToken::new(), |e| {
            if matches!(e, OpEvent::Sent { .. }) {
                sent += 1;
            }
        })
        .await
        .unwrap();
    assert_eq!(sent, 2);
    assert_eq!(view(&store, "Label_9"), ["t2"]);
    assert!(view(&store, local.as_str()).is_empty());
    assert_eq!(
        store.label(&LabelId::new("Label_9")).unwrap().unwrap().name,
        "Receipts"
    );
}

#[tokio::test]
async fn undoing_a_label_creation_undoes_its_uses() {
    let (_server, store, client, _) = synced().await;
    let queue = OpsQueue::new(client.clone(), OpsConfig::default());
    let create = queue
        .enqueue(
            &store,
            Op::CreateLabel {
                name: "Receipts".to_owned(),
                color: None,
            },
        )
        .unwrap();
    let local = queue.local_label(&store, create).unwrap().unwrap();
    queue
        .enqueue(&store, Op::modify(thread("t2"), [local.clone()], []))
        .unwrap();
    queue.enqueue(&store, Op::star(thread("t1"))).unwrap();

    assert!(queue.undo(&store, create).unwrap());
    assert!(store.label(&local).unwrap().is_none());
    assert_eq!(labels_of(&store, "m2"), ["INBOX"]);
    let left = queue.queued(&store).unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].op, Op::star(thread("t1")));
}

#[tokio::test]
async fn pending_ops_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("mail.db");
    let (server, memory, client, _) = synced().await;
    // Copy the synced mailbox into a file database.
    {
        let store = Store::open(&db).unwrap();
        for label in memory.labels().unwrap() {
            store.upsert_label(&label).unwrap();
        }
        let threads: Vec<_> = ["t1", "t2"]
            .iter()
            .map(|id| {
                let id = ThreadId::new(*id);
                let thread = memory.thread(&id).unwrap().unwrap();
                let messages = memory
                    .thread_message_ids(&id)
                    .unwrap()
                    .iter()
                    .map(|m| memory.message(m).unwrap().unwrap())
                    .collect();
                (thread, messages)
            })
            .collect();
        store.replace_threads(&threads).unwrap();

        let queue = instant_queue(&client);
        let archive = queue.enqueue(&store, Op::archive(thread("t1"))).unwrap();
        queue.enqueue(&store, Op::star(thread("t2"))).unwrap();
        // The app stopped while sending the archive.
        assert!(store.claim_pending_op(archive.get()).unwrap());
    }

    batch_modify(json!({"ids": ["m1"], "removeLabelIds": ["INBOX"]}))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    batch_modify(json!({"ids": ["m2"], "addLabelIds": ["STARRED"]}))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    let mut store = Store::open(&db).unwrap();
    assert_eq!(view(&store, "INBOX"), ["t2"]);
    let queue = instant_queue(&client);
    let mut sent = Vec::new();
    queue
        .drain(&mut store, &CancellationToken::new(), |e| {
            if let OpEvent::Sent { op, .. } = e {
                sent.push(op);
            }
        })
        .await
        .unwrap();
    assert_eq!(sent, [Op::archive(thread("t1")), Op::star(thread("t2"))]);
    assert_eq!(view(&store, "STARRED"), ["t2"]);
}
