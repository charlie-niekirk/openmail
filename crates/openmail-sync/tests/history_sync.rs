//! Incremental sync from `history.list` against a `wiremock` Gmail and an
//! in-memory store.

mod common;

use std::time::Duration;

use common::{
    BatchResponder, FakeThread, Listing, Mailbox, NEWEST_MS, PROFILE_HISTORY_ID, get, history_list,
    history_page, labels_added, labels_removed, list_page, message_added, message_deleted,
    mount_labels, mount_profile, page, setup,
};
use openmail_core::{HistoryId, LabelId, MessageId, ThreadId};
use openmail_gmail::GmailClient;
use openmail_store::{Store, ThreadView};
use openmail_sync::{
    CancellationToken, Changes, Cutoff, Error, HistorySync, InitialSync, InitialSyncConfig,
    Scheduler, SchedulerConfig, SyncEvent,
};
use serde_json::json;
use wiremock::{MockServer, ResponseTemplate};

const START: u64 = PROFILE_HISTORY_ID;

/// Inbox: t1 (unread), t2, t3. Elsewhere: t4 (labelled), t5.
fn mailbox() -> Mailbox {
    Mailbox::default()
        .with(1, &["INBOX", "UNREAD"])
        .with(2, &["INBOX"])
        .with(3, &["INBOX", "Label_1"])
        .with(4, &["Label_1"])
        .with(5, &[])
}

fn config() -> InitialSyncConfig {
    InitialSyncConfig {
        cutoff: Cutoff::All,
        rate_limit_pause: Duration::from_millis(10),
        ..InitialSyncConfig::default()
    }
}

/// A mock Gmail serving `mailbox()`, and a store fully synced from it at
/// history ID [`START`].
async fn synced() -> (MockServer, InitialSync, BatchResponder, GmailClient) {
    let (server, client) = setup().await;
    let mailbox = mailbox();
    mount_profile(&server, 1).await;
    mount_labels(&server).await;
    list_page(Listing::Inbox, None)
        .respond_with(page(&mailbox, &["t1", "t2", "t3"], None))
        .mount(&server)
        .await;
    list_page(Listing::Backfill, None)
        .respond_with(page(&mailbox, &["t1", "t2", "t3", "t4", "t5"], None))
        .mount(&server)
        .await;
    let batch = BatchResponder::new(mailbox);
    batch.mount(&server).await;
    let mut sync = InitialSync::new(client.clone(), Store::open_in_memory().unwrap(), config());
    sync.run(&CancellationToken::new(), |_| {}).await.unwrap();
    (server, sync, batch, client)
}

/// Runs one incremental sync.
async fn run(sync: &mut InitialSync, client: &GmailClient) -> Result<Changes, Error> {
    HistorySync::new(client.clone())
        .run(sync.store_mut(), &CancellationToken::new())
        .await
}

fn history_id(store: &Store) -> Option<HistoryId> {
    store.sync_state().unwrap().history_id
}

fn thread_ids(store: &Store, label: &str) -> Vec<String> {
    let mut ids: Vec<String> = store
        .thread_page(&ThreadView::Label(LabelId::new(label)), None, 100)
        .unwrap()
        .threads
        .into_iter()
        .map(|t| t.id.as_str().to_owned())
        .collect();
    ids.sort();
    ids
}

fn labels(store: &Store, message: &str) -> Vec<String> {
    store
        .message_label_ids(&MessageId::new(message))
        .unwrap()
        .into_iter()
        .map(|l| l.as_str().to_owned())
        .collect()
}

#[tokio::test]
async fn new_messages_fetch_their_threads() {
    let (server, mut sync, batch, client) = synced().await;
    // A new thread t6, and a reply m1b in t1.
    let mut changed = mailbox().with(6, &["INBOX", "UNREAD"]);
    let t1 = changed.threads.get_mut("t1").unwrap();
    t1.history_id = START + 2;
    t1.messages.push((
        "m1b".to_owned(),
        vec!["INBOX".to_owned()],
        NEWEST_MS + 60_000,
    ));
    batch.set_mailbox(changed);
    history_list(START, None)
        .respond_with(history_page(
            vec![
                message_added(START + 1, "m6", "t6", &["INBOX", "UNREAD"]),
                message_added(START + 2, "m1b", "t1", &["INBOX"]),
            ],
            START + 3,
            None,
        ))
        .expect(1)
        .mount(&server)
        .await;

    let changes = run(&mut sync, &client).await.unwrap();

    assert_eq!(
        changes,
        Changes {
            pages: 1,
            records: 2,
            threads_fetched: 2,
            history_id: Some(HistoryId::new(START + 3)),
            ..Changes::default()
        }
    );
    let store = sync.store();
    assert_eq!(history_id(store), Some(HistoryId::new(START + 3)));
    assert_eq!(thread_ids(store, "INBOX"), ["t1", "t2", "t3", "t6"]);
    let t1 = store
        .thread_page(&ThreadView::Label(LabelId::new("INBOX")), None, 1)
        .unwrap()
        .threads
        .remove(0);
    // t1 is now the newest, with two messages and the reply's snippet.
    assert_eq!(t1.id, ThreadId::new("t1"));
    assert_eq!(t1.message_count, 2);
    assert_eq!(t1.snippet, "snippet of m1b");
    assert!(t1.is_unread);
}

#[tokio::test]
async fn deleted_messages_refetch_or_delete_their_threads() {
    let (server, mut sync, batch, client) = synced().await;
    // t2 is gone entirely; t1 loses one of its two messages.
    let mut two = mailbox();
    two.threads.get_mut("t1").unwrap().messages.push((
        "m1b".to_owned(),
        vec!["INBOX".to_owned(), "STARRED".to_owned()],
        NEWEST_MS,
    ));
    sync.store_mut()
        .replace_threads(&[thread_with(&two, "t1")])
        .unwrap();
    assert_eq!(thread_ids(sync.store(), "STARRED"), ["t1"]);
    let mut changed = mailbox();
    changed.threads.remove("t2");
    changed.threads.get_mut("t1").unwrap().history_id = START + 2;
    batch.set_mailbox(changed);
    history_list(START, None)
        .respond_with(history_page(
            vec![
                message_deleted(START + 1, "m2", "t2"),
                message_deleted(START + 2, "m1b", "t1"),
                // Not stored: nothing to do.
                message_deleted(START + 3, "m99", "t99"),
            ],
            START + 4,
            None,
        ))
        .mount(&server)
        .await;

    let changes = run(&mut sync, &client).await.unwrap();

    assert_eq!(changes.threads_fetched, 1);
    assert_eq!(changes.threads_deleted, 1);
    let store = sync.store();
    assert_eq!(thread_ids(store, "INBOX"), ["t1", "t3"]);
    assert!(store.message(&MessageId::new("m1b")).unwrap().is_none());
    assert!(thread_ids(store, "STARRED").is_empty());
    assert_eq!(
        batch
            .requested("threads")
            .iter()
            .filter(|t| *t == "t99")
            .count(),
        0
    );
}

/// `(Thread, Messages)` for a thread of `mailbox`, the way a fetch stores it.
fn thread_with(
    mailbox: &Mailbox,
    id: &str,
) -> (openmail_core::Thread, Vec<openmail_core::Message>) {
    let FakeThread {
        history_id,
        messages,
    } = &mailbox.threads[id];
    let messages: Vec<openmail_core::Message> = messages
        .iter()
        .map(|(mid, labels, date)| openmail_core::Message {
            id: MessageId::new(mid),
            thread_id: ThreadId::new(id),
            history_id: HistoryId::new(*history_id),
            label_ids: labels.iter().map(|l| LabelId::new(l.as_str())).collect(),
            snippet: String::new(),
            internal_date: time::OffsetDateTime::from_unix_timestamp(date / 1000).unwrap(),
            size_estimate: 1,
            headers: openmail_core::MessageHeaders::default(),
            has_attachments: false,
        })
        .collect();
    let thread = openmail_core::Thread {
        id: ThreadId::new(id),
        history_id: HistoryId::new(*history_id),
        subject: None,
        snippet: String::new(),
        last_message_date: messages.last().unwrap().internal_date,
        message_count: messages.len() as u32,
        label_ids: Vec::new(),
        senders: Vec::new(),
        has_attachments: false,
    };
    (thread, messages)
}

#[tokio::test]
async fn label_changes_update_messages_and_thread_rollups() {
    let (server, mut sync, batch, client) = synced().await;
    history_list(START, None)
        .respond_with(history_page(
            vec![
                // Read t1, star t2, archive t3.
                labels_removed(START + 1, "m1", "t1", &["UNREAD"]),
                labels_added(START + 2, "m2", "t2", &["STARRED"]),
                labels_removed(START + 3, "m3", "t3", &["INBOX"]),
                // Outside the synced range: ignored.
                labels_added(START + 4, "m99", "t99", &["STARRED"]),
            ],
            START + 5,
            None,
        ))
        .mount(&server)
        .await;

    let batches = batch.calls().len();
    let changes = run(&mut sync, &client).await.unwrap();

    assert_eq!(
        changes,
        Changes {
            pages: 1,
            records: 4,
            label_changes: 3,
            history_id: Some(HistoryId::new(START + 5)),
            ..Changes::default()
        }
    );
    // No thread was fetched.
    assert_eq!(batch.calls().len(), batches);
    let store = sync.store();
    assert_eq!(thread_ids(store, "INBOX"), ["t1", "t2"]);
    assert!(thread_ids(store, "UNREAD").is_empty());
    assert_eq!(thread_ids(store, "STARRED"), ["t2"]);
    assert_eq!(labels(store, "m3"), ["Label_1"]);
    let counts = store.label_counts().unwrap();
    let inbox = counts
        .iter()
        .find(|c| c.label_id.as_str() == "INBOX")
        .unwrap();
    assert_eq!((inbox.threads_total, inbox.threads_unread), (2, 0));
}

#[tokio::test]
async fn an_unknown_label_syncs_the_label_list_first() {
    let (server, mut sync, _batch, client) = synced().await;
    get("labels")
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "labels": [
                {"id": "INBOX", "name": "INBOX", "type": "system"},
                {"id": "UNREAD", "name": "UNREAD", "type": "system"},
                {"id": "Label_1", "name": "Work", "type": "user"},
                {"id": "Label_2", "name": "Travel", "type": "user"},
            ]
        })))
        .with_priority(1)
        .expect(1)
        .mount(&server)
        .await;
    history_list(START, None)
        .respond_with(history_page(
            vec![labels_added(START + 1, "m5", "t5", &["Label_2"])],
            START + 1,
            None,
        ))
        .mount(&server)
        .await;

    let changes = run(&mut sync, &client).await.unwrap();

    assert!(changes.labels_synced);
    assert_eq!(changes.label_changes, 1);
    let store = sync.store();
    let travel = store.label(&LabelId::new("Label_2")).unwrap().unwrap();
    assert_eq!(travel.name, "Travel");
    assert_eq!(thread_ids(store, "Label_2"), ["t5"]);
}

#[tokio::test]
async fn moving_an_unsynced_thread_to_the_inbox_fetches_it() {
    let (server, mut sync, batch, client) = synced().await;
    batch.set_mailbox(mailbox().with(400, &["INBOX"]));
    history_list(START, None)
        .respond_with(history_page(
            vec![labels_added(START + 1, "m400", "t400", &["INBOX"])],
            START + 1,
            None,
        ))
        .mount(&server)
        .await;

    let changes = run(&mut sync, &client).await.unwrap();

    assert_eq!(changes.threads_fetched, 1);
    assert_eq!(changes.label_changes, 0);
    assert_eq!(
        thread_ids(sync.store(), "INBOX"),
        ["t1", "t2", "t3", "t400"]
    );
}

#[tokio::test]
async fn pages_advance_the_history_id_one_at_a_time() {
    let (server, mut sync, _batch, client) = synced().await;
    history_list(START, None)
        .respond_with(history_page(
            vec![
                labels_removed(START + 1, "m1", "t1", &["UNREAD"]),
                labels_added(START + 3, "m2", "t2", &["STARRED"]),
            ],
            START + 9,
            Some("p2"),
        ))
        .mount(&server)
        .await;
    // The second page fails the first time.
    history_list(START, Some("p2"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    history_list(START, Some("p2"))
        .respond_with(history_page(
            vec![labels_removed(START + 7, "m3", "t3", &["INBOX"])],
            START + 9,
            None,
        ))
        .mount(&server)
        .await;

    let err = run(&mut sync, &client).await.unwrap_err();
    assert!(err.is_transient(), "{err:?}");
    // The first page is applied, and the history ID is at its last record.
    let store = sync.store();
    assert_eq!(history_id(store), Some(HistoryId::new(START + 3)));
    assert_eq!(thread_ids(store, "STARRED"), ["t2"]);
    assert_eq!(thread_ids(store, "INBOX"), ["t1", "t2", "t3"]);

    // The next run continues from there.
    history_list(START + 3, None)
        .respond_with(history_page(
            vec![labels_removed(START + 7, "m3", "t3", &["INBOX"])],
            START + 9,
            None,
        ))
        .expect(1)
        .mount(&server)
        .await;
    let changes = run(&mut sync, &client).await.unwrap();
    assert_eq!(changes.history_id, Some(HistoryId::new(START + 9)));
    assert_eq!(thread_ids(sync.store(), "INBOX"), ["t1", "t2"]);
}

#[tokio::test]
async fn multiple_pages_are_applied_in_one_run() {
    let (server, mut sync, _batch, client) = synced().await;
    history_list(START, None)
        .respond_with(history_page(
            vec![labels_added(START + 1, "m4", "t4", &["STARRED"])],
            START + 4,
            Some("p2"),
        ))
        .expect(1)
        .mount(&server)
        .await;
    history_list(START, Some("p2"))
        .respond_with(history_page(
            vec![labels_removed(START + 2, "m4", "t4", &["STARRED"])],
            START + 4,
            None,
        ))
        .expect(1)
        .mount(&server)
        .await;

    let changes = run(&mut sync, &client).await.unwrap();
    assert_eq!(changes.pages, 2);
    assert_eq!(changes.label_changes, 2);
    assert_eq!(changes.history_id, Some(HistoryId::new(START + 4)));
    assert!(thread_ids(sync.store(), "STARRED").is_empty());
}

#[tokio::test]
async fn replaying_a_page_gives_the_same_state() {
    let (server, mut sync, batch, client) = synced().await;
    let mut changed = mailbox().with(6, &["INBOX"]);
    changed.threads.remove("t5");
    batch.set_mailbox(changed);
    history_list(START, None)
        .respond_with(history_page(
            vec![
                message_added(START + 1, "m6", "t6", &["INBOX"]),
                message_deleted(START + 2, "m5", "t5"),
                labels_added(START + 3, "m2", "t2", &["STARRED", "UNREAD"]),
                labels_removed(START + 4, "m1", "t1", &["UNREAD", "INBOX"]),
            ],
            START + 5,
            None,
        ))
        .mount(&server)
        .await;

    run(&mut sync, &client).await.unwrap();
    let snapshot = |store: &Store| {
        (
            ["INBOX", "UNREAD", "STARRED", "Label_1"].map(|l| thread_ids(store, l)),
            store.label_counts().unwrap(),
            store
                .thread_page(&ThreadView::AllMail, None, 100)
                .unwrap()
                .threads,
        )
    };
    let first = snapshot(sync.store());

    // Rewind the history ID and apply the same page again.
    let store = sync.store_mut();
    let mut state = store.sync_state().unwrap();
    state.history_id = Some(HistoryId::new(START));
    store.set_sync_state(&state).unwrap();
    run(&mut sync, &client).await.unwrap();

    assert_eq!(snapshot(sync.store()), first);
    assert_eq!(history_id(sync.store()), Some(HistoryId::new(START + 5)));
}

#[tokio::test]
async fn expired_history_resets_for_a_full_resync() {
    let (server, mut sync, _batch, client) = synced().await;
    history_list(START, None)
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "error": {"code": 404, "message": "Requested entity was not found."}
        })))
        .mount(&server)
        .await;

    let changes = run(&mut sync, &client).await.unwrap();

    assert!(changes.full_resync);
    assert_eq!(changes.history_id, None);
    let state = sync.store().sync_state().unwrap();
    assert_eq!(state.history_id, None);
    assert!(!state.backfill_complete);
    // Stored rows are kept until the resync.
    assert_eq!(thread_ids(sync.store(), "INBOX"), ["t1", "t2", "t3"]);
    // And incremental sync refuses to run until it has happened.
    assert!(matches!(
        run(&mut sync, &client).await,
        Err(Error::InitialSyncIncomplete)
    ));
}

#[tokio::test]
async fn needs_a_finished_initial_sync() {
    let (server, client) = setup().await;
    let mut store = Store::open_in_memory().unwrap();
    let result = HistorySync::new(client)
        .run(&mut store, &CancellationToken::new())
        .await;
    assert!(matches!(result, Err(Error::InitialSyncIncomplete)));
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn the_history_future_is_send() {
    fn assert_send<T: Send>(_: T) {}
    let (_server, client) = setup().await;
    let mut store = Store::open_in_memory().unwrap();
    let sync = HistorySync::new(client);
    assert_send(sync.run(&mut store, &CancellationToken::new()));
}

#[tokio::test]
async fn the_scheduler_syncs_then_polls_history() {
    let (server, client) = setup().await;
    let mailbox = mailbox();
    mount_profile(&server, 1).await;
    mount_labels(&server).await;
    list_page(Listing::Inbox, None)
        .respond_with(page(&mailbox, &["t1", "t2", "t3"], None))
        .mount(&server)
        .await;
    list_page(Listing::Backfill, None)
        .respond_with(page(&mailbox, &["t1", "t2", "t3", "t4", "t5"], None))
        .mount(&server)
        .await;
    BatchResponder::new(mailbox).mount(&server).await;
    // The first poll stars t2; later ones find nothing new.
    history_list(START, None)
        .respond_with(history_page(
            vec![labels_added(START + 1, "m2", "t2", &["STARRED"])],
            START + 1,
            None,
        ))
        .expect(1)
        .mount(&server)
        .await;
    history_list(START + 1, None)
        .respond_with(history_page(vec![], START + 1, None))
        .mount(&server)
        .await;

    let schedule = SchedulerConfig {
        active_interval: Duration::from_millis(20),
        ..SchedulerConfig::default()
    };
    let mut scheduler =
        Scheduler::new(client, Store::open_in_memory().unwrap(), config(), schedule);
    let cancel = CancellationToken::new();
    let mut polls = Vec::new();
    let mut initial = 0;
    scheduler
        .run(&cancel, |event| match event {
            SyncEvent::Initial(_) => initial += 1,
            SyncEvent::Polled(changes) => {
                polls.push(changes);
                if polls.len() == 3 {
                    cancel.cancel();
                }
            }
            other => assert!(matches!(other, SyncEvent::Connectivity(_)), "{other:?}"),
        })
        .await
        .unwrap();

    assert!(initial > 0);
    assert_eq!(polls[0].label_changes, 1);
    assert!(polls[1].is_empty() && polls[2].is_empty());
    assert_eq!(thread_ids(scheduler.store(), "STARRED"), ["t2"]);
    assert_eq!(
        history_id(scheduler.store()),
        Some(HistoryId::new(START + 1))
    );
}
