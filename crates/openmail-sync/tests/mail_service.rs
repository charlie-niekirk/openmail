//! `MailService` against a `wiremock` Gmail: syncing, queries, ops and
//! shutdown.

mod common;

use std::time::Duration;

use common::{
    BatchResponder, Listing, Mailbox, PROFILE_HISTORY_ID, USER, get, history_list, history_page,
    list_page, mount_labels, mount_profile, page, setup,
};
use openmail_core::{LabelId, ThreadId};
use openmail_gmail::GmailClient;
use openmail_store::ThreadView;
use openmail_sync::{
    Connectivity, Cutoff, Error, InitialSyncConfig, MailEvent, MailEvents, MailHandle, MailService,
    MailServiceConfig, Op, OpEvent, OpsConfig, SchedulerConfig, SyncStatus, Target,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// t1 (unread) and t2 in the Inbox, t3 starred and archived.
fn mailbox() -> Mailbox {
    Mailbox::default()
        .with(1, &["INBOX", "UNREAD"])
        .with(2, &["INBOX"])
        .with(3, &["STARRED"])
}

/// A mock Gmail serving `mailbox()`: the initial sync, empty history and
/// `batchModify`.
async fn gmail() -> (MockServer, GmailClient, BatchResponder) {
    let (server, client) = setup().await;
    let mailbox = mailbox();
    mount_profile(&server, 1).await;
    mount_labels(&server).await;
    list_page(Listing::Inbox, None)
        .respond_with(page(&mailbox, &["t1", "t2"], None))
        .mount(&server)
        .await;
    list_page(Listing::Backfill, None)
        .respond_with(page(&mailbox, &["t1", "t2", "t3"], None))
        .mount(&server)
        .await;
    history_list(PROFILE_HISTORY_ID, None)
        .respond_with(history_page(vec![], PROFILE_HISTORY_ID, None))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{USER}/messages/batchModify")))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    let batch = BatchResponder::new(mailbox);
    batch.mount(&server).await;
    (server, client, batch)
}

/// Settings for tests: everything since the epoch, one poll, and `undo` as
/// the undo window.
fn config(dir: &TempDir, undo: Duration) -> MailServiceConfig {
    MailServiceConfig {
        initial: InitialSyncConfig {
            cutoff: Cutoff::All,
            ..InitialSyncConfig::default()
        },
        scheduler: SchedulerConfig {
            active_interval: Duration::from_secs(3600),
            idle_interval: Duration::from_secs(3600),
            ..SchedulerConfig::default()
        },
        ops: OpsConfig {
            undo_window: undo,
            initial_backoff: Duration::ZERO,
            ..OpsConfig::default()
        },
        ..MailServiceConfig::new(dir.path().join("mail.db"))
    }
}

/// Receives events until one matches `done`, returning them all.
async fn until(events: &mut MailEvents, done: impl Fn(&MailEvent) -> bool) -> Vec<MailEvent> {
    let mut seen = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = events.recv().await.expect("the service stopped");
            let finished = done(&event);
            seen.push(event);
            if finished {
                return;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out; events so far: {seen:#?}"));
    seen
}

fn up_to_date(event: &MailEvent) -> bool {
    matches!(event, MailEvent::SyncStatus(SyncStatus::UpToDate(_)))
}

/// Waits for the first sync to finish: up to date, then online (which the
/// scheduler reports after its first successful attempt).
async fn synced(events: &mut MailEvents) -> Vec<MailEvent> {
    let mut seen = until(events, up_to_date).await;
    seen.extend(
        until(events, |e| {
            matches!(e, MailEvent::Connectivity(Connectivity::Online))
        })
        .await,
    );
    seen
}

async fn inbox(handle: &MailHandle) -> Vec<String> {
    view(handle, "INBOX").await
}

async fn view(handle: &MailHandle, label: &str) -> Vec<String> {
    let page = handle
        .thread_page(ThreadView::Label(LabelId::new(label)), None, 50)
        .await
        .unwrap();
    page.threads
        .into_iter()
        .map(|t| t.id.as_str().to_owned())
        .collect()
}

fn thread(id: &str) -> Target {
    Target::thread(ThreadId::new(id))
}

#[tokio::test]
async fn syncs_in_the_background_and_answers_queries() {
    let (_server, client, _batch) = gmail().await;
    let dir = TempDir::new().unwrap();
    let (service, mut events) = MailService::start(client, config(&dir, Duration::ZERO)).unwrap();
    let handle = service.handle();

    let seen = synced(&mut events).await;
    assert!(
        seen.iter()
            .any(|e| matches!(e, MailEvent::SyncStatus(SyncStatus::Initial(_))))
    );
    assert!(seen.iter().any(|e| matches!(e, MailEvent::LabelsChanged)));
    assert!(seen.iter().any(|e| matches!(e, MailEvent::ThreadsChanged)));

    assert_eq!(inbox(&handle).await, ["t1", "t2"]);
    let labels = handle.labels().await.unwrap();
    assert!(labels.iter().any(|l| l.id.as_str() == "Label_1"));
    let counts = handle.label_counts().await.unwrap();
    let inbox_counts = counts
        .iter()
        .find(|c| c.label_id.as_str() == "INBOX")
        .unwrap();
    assert_eq!(
        (inbox_counts.threads_total, inbox_counts.threads_unread),
        (2, 1)
    );
    let detail = handle.thread(ThreadId::new("t1")).await.unwrap().unwrap();
    assert_eq!(detail.messages.len(), 1);
    assert_eq!(detail.messages[0].message.id.as_str(), "m1");
    assert!(
        handle
            .thread(ThreadId::new("nope"))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn applies_ops_at_once_and_sends_them() {
    let (server, client, _batch) = gmail().await;
    let dir = TempDir::new().unwrap();
    let (service, mut events) = MailService::start(client, config(&dir, Duration::ZERO)).unwrap();
    let handle = service.handle();
    synced(&mut events).await;

    let id = handle.apply(Op::archive(thread("t1"))).await.unwrap();
    assert_eq!(inbox(&handle).await, ["t2"]);
    let seen = until(
        &mut events,
        |e| matches!(e, MailEvent::Op(OpEvent::Sent { id: sent, .. }) if *sent == id),
    )
    .await;
    assert!(matches!(seen[0], MailEvent::ThreadsChanged));

    let sent: Vec<Value> = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path().ends_with("/messages/batchModify"))
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(sent, [json!({"ids": ["m1"], "removeLabelIds": ["INBOX"]})]);
}

#[tokio::test]
async fn undo_reverts_an_op_within_its_window() {
    let (server, client, _batch) = gmail().await;
    let dir = TempDir::new().unwrap();
    let config = config(&dir, Duration::from_secs(60));
    let (service, mut events) = MailService::start(client, config).unwrap();
    let handle = service.handle();
    synced(&mut events).await;

    let id = handle.apply(Op::star(thread("t2"))).await.unwrap();
    assert_eq!(view(&handle, "STARRED").await, ["t2", "t3"]);
    assert_eq!(handle.queued_ops().await.unwrap().len(), 1);
    assert!(handle.undo(id).await.unwrap());
    assert_eq!(view(&handle, "STARRED").await, ["t3"]);
    assert!(handle.queued_ops().await.unwrap().is_empty());
    // Too late now: it is gone.
    assert!(!handle.undo(id).await.unwrap());

    assert!(matches!(
        events.recv().await,
        Some(MailEvent::ThreadsChanged)
    ));
    assert!(matches!(
        events.recv().await,
        Some(MailEvent::ThreadsChanged)
    ));
    drop(service);
    let modified = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().ends_with("/messages/batchModify"))
        .count();
    assert_eq!(modified, 0);
}

#[tokio::test]
async fn shutting_down_ends_the_events_and_the_handles() {
    let (_server, client, _batch) = gmail().await;
    let dir = TempDir::new().unwrap();
    let (service, mut events) = MailService::start(client, config(&dir, Duration::ZERO)).unwrap();
    let handle = service.handle();
    synced(&mut events).await;

    service.shutdown();
    while events.recv().await.is_some() {}
    assert!(matches!(handle.labels().await, Err(Error::ServiceStopped)));
    assert!(matches!(
        handle.apply(Op::archive(thread("t1"))).await,
        Err(Error::ServiceStopped)
    ));
    // The calls after shutdown are no-ops.
    handle.poll_now();
}

#[tokio::test]
async fn resumes_from_the_store_after_a_restart() {
    let (server, client, _batch) = gmail().await;
    let dir = TempDir::new().unwrap();
    let (service, mut events) =
        MailService::start(client.clone(), config(&dir, Duration::from_secs(60))).unwrap();
    synced(&mut events).await;
    let handle = service.handle();
    handle.apply(Op::archive(thread("t1"))).await.unwrap();
    service.shutdown();

    // The second start polls history instead of syncing again, and keeps
    // the op queued before the restart (still in its undo window), with its
    // local change.
    let config = config(&dir, Duration::from_secs(60));
    let (service, mut events) = MailService::start(client, config).unwrap();
    let handle = service.handle();
    assert_eq!(inbox(&handle).await, ["t2"]);
    let seen = synced(&mut events).await;
    assert!(
        !seen
            .iter()
            .any(|e| matches!(e, MailEvent::SyncStatus(SyncStatus::Initial(_))))
    );
    assert_eq!(handle.queued_ops().await.unwrap().len(), 1);
    drop(service);
    // `mount_profile` expects one call: the profile was not fetched again.
    server.verify().await;
}

#[tokio::test]
async fn a_sign_in_failure_stops_syncing_but_not_queries() {
    let (server, client) = setup().await;
    get("profile")
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let dir = TempDir::new().unwrap();
    let (service, mut events) = MailService::start(client, config(&dir, Duration::ZERO)).unwrap();
    let handle = service.handle();

    let seen = until(&mut events, |e| {
        matches!(e, MailEvent::SyncStatus(SyncStatus::Stopped(_)))
    })
    .await;
    let Some(MailEvent::SyncStatus(SyncStatus::Stopped(error))) = seen.last() else {
        unreachable!()
    };
    assert!(!error.is_transient());
    assert!(inbox(&handle).await.is_empty());
}
