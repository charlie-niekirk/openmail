//! The initial sync against a `wiremock` Gmail and an in-memory store.

mod common;

use std::time::{Duration, Instant};

use common::{
    BatchResponder, Listing, Mailbox, PROFILE_HISTORY_ID, list_page, mount_labels, mount_profile,
    page, setup, setup_retrying,
};
use openmail_core::{HistoryId, LabelId, MessageId, ThreadId};
use openmail_gmail::api;
use openmail_store::Store;
use openmail_sync::{
    CancellationToken, Cutoff, Error, InitialSync, InitialSyncConfig, Phase, Progress,
};
use serde_json::Value;
use time::OffsetDateTime;
use time::macros::datetime;
use wiremock::matchers::{query_param, query_param_is_missing};
use wiremock::{MockServer, ResponseTemplate};

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
        page_size: 2,
        prefetch_inbox_bodies: None,
        rate_limit_pause: Duration::from_millis(10),
        max_rate_limit_pauses: 5,
    }
}

/// Inbox pages [t1, t2] → [t3]. Each is expected once.
async fn mount_inbox(server: &MockServer, mailbox: &Mailbox) {
    list_page(Listing::Inbox, None)
        .respond_with(page(mailbox, &["t1", "t2"], Some("i2")))
        .expect(1)
        .mount(server)
        .await;
    list_page(Listing::Inbox, Some("i2"))
        .respond_with(page(mailbox, &["t3"], None))
        .expect(1)
        .mount(server)
        .await;
}

/// Backfill pages [t3, t4] → [t5] (t3 is already stored by then). Each is
/// expected once.
async fn mount_backfill(server: &MockServer, mailbox: &Mailbox) {
    list_page(Listing::Backfill, None)
        .respond_with(page(mailbox, &["t3", "t4"], Some("b2")))
        .expect(1)
        .mount(server)
        .await;
    list_page(Listing::Backfill, Some("b2"))
        .respond_with(page(mailbox, &["t5"], None))
        .expect(1)
        .mount(server)
        .await;
}

/// Mounts the whole mailbox and returns the batch responder.
async fn mount_all(server: &MockServer) -> BatchResponder {
    let mailbox = mailbox();
    mount_profile(server, 1).await;
    mount_labels(server).await;
    mount_inbox(server, &mailbox).await;
    mount_backfill(server, &mailbox).await;
    let batch = BatchResponder::new(mailbox);
    batch.mount(server).await;
    batch
}

/// Runs a sync, recording every progress report.
async fn run(
    sync: &mut InitialSync,
    cancel: &CancellationToken,
) -> (Result<Progress, Error>, Vec<Progress>) {
    let mut seen = Vec::new();
    let result = sync.run(cancel, |p| seen.push(p.clone())).await;
    (result, seen)
}

/// The distinct phases in the order they were reported.
fn phases(seen: &[Progress]) -> Vec<Phase> {
    let mut phases: Vec<Phase> = seen.iter().map(|p| p.phase).collect();
    phases.dedup();
    phases
}

fn stored(store: &Store, thread: &str) -> bool {
    store.thread(&ThreadId::new(thread)).unwrap().is_some()
}

/// The stored checkpoint as JSON.
fn checkpoint(store: &Store) -> Value {
    let cursor = store
        .sync_state()
        .unwrap()
        .backfill_cursor
        .expect("a checkpoint");
    serde_json::from_str(&cursor).unwrap()
}

#[tokio::test]
async fn full_run_syncs_labels_inbox_and_backfill() {
    let (server, client) = setup().await;
    let batch = mount_all(&server).await;
    let mut sync = InitialSync::new(client, Store::open_in_memory().unwrap(), config());

    let (result, seen) = run(&mut sync, &CancellationToken::new()).await;
    let done = result.unwrap();

    assert_eq!(
        phases(&seen),
        [Phase::Labels, Phase::Inbox, Phase::Backfill, Phase::Done]
    );
    assert_eq!(
        done,
        Progress {
            phase: Phase::Done,
            resumed: false,
            labels: 3,
            threads: 5,
            messages: 5,
            skipped_threads: 1,
            bodies: 0,
            deleted_threads: 0,
            rate_limit_wait: None,
            retried_requests: 0,
            rate_limited_requests: 0,
            // The default adaptive rate, with no rate limits to cut it.
            units_per_second: Some(150),
        }
    );
    // Counts grow page by page.
    let threads: Vec<u64> = seen.iter().map(|p| p.threads).collect();
    assert!(threads.windows(2).all(|w| w[0] <= w[1]), "{threads:?}");

    let store = sync.store();
    let state = store.sync_state().unwrap();
    assert_eq!(state.history_id, Some(HistoryId::new(PROFILE_HISTORY_ID)));
    assert!(state.backfill_complete);
    assert_eq!(state.backfill_cursor, None);
    assert!(state.last_full_sync.is_some());

    let names: Vec<_> = store
        .labels()
        .unwrap()
        .into_iter()
        .map(|l| l.name)
        .collect();
    assert_eq!(names, ["INBOX", "UNREAD", "Work"]);

    for t in ["t1", "t2", "t3", "t4", "t5"] {
        assert!(stored(store, t), "{t}");
    }
    let t1 = store.thread(&ThreadId::new("t1")).unwrap().unwrap();
    assert!(t1.is_unread());
    assert_eq!(t1.subject.as_deref(), Some("Subject t1"));
    assert_eq!(t1.history_id, HistoryId::new(101));
    let m4 = store.message(&MessageId::new("m4")).unwrap().unwrap();
    assert_eq!(m4.label_ids, [LabelId::new("Label_1")]);
    assert_eq!(m4.headers.to[0].email, "bob@example.com");
    assert_eq!(
        m4.headers.message_id_header.as_deref(),
        Some("<m4@example.com>")
    );
    assert!(store.body(&MessageId::new("m1")).unwrap().is_none());

    // One batch per page, t3 fetched once, with metadata and only the sync headers.
    assert_eq!(batch.requested("threads"), ["t1", "t2", "t3", "t4", "t5"]);
    assert_eq!(batch.calls().len(), 4);
    for item in batch.calls().iter().flatten() {
        let query = item.target.split_once('?').unwrap().1;
        assert_eq!(
            query,
            "format=metadata&metadataHeaders=From&metadataHeaders=To&metadataHeaders=Cc\
             &metadataHeaders=Subject&metadataHeaders=Date&metadataHeaders=Message-ID"
        );
    }
    // Pages use the configured size, and Cutoff::All sends no query.
    let lists: Vec<_> = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path().ends_with("/threads"))
        .collect();
    assert_eq!(lists.len(), 4);
    for request in &lists {
        let pairs: Vec<(String, String)> = request.url.query_pairs().into_owned().collect();
        assert!(pairs.contains(&("maxResults".to_owned(), "2".to_owned())));
        assert!(!pairs.iter().any(|(k, _)| k == "q"));
    }
}

#[tokio::test]
async fn a_finished_sync_does_nothing_when_run_again() {
    let (server, client) = setup().await;
    mount_all(&server).await;
    let mut sync = InitialSync::new(client, Store::open_in_memory().unwrap(), config());
    sync.run(&CancellationToken::new(), |_| {}).await.unwrap();
    let requests = server.received_requests().await.unwrap().len();

    let again = sync.run(&CancellationToken::new(), |_| {}).await.unwrap();
    assert_eq!(
        again,
        Progress {
            phase: Phase::Done,
            units_per_second: again.units_per_second,
            ..Progress::default()
        }
    );
    assert_eq!(server.received_requests().await.unwrap().len(), requests);
}

#[tokio::test]
async fn resumes_mid_backfill_after_a_failure() {
    let (server, client) = setup().await;
    // The second backfill page fails once, then works.
    list_page(Listing::Backfill, Some("b2"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": {"code": 400, "message": "boom"}
        })))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    let batch = mount_all(&server).await;

    let mut sync = InitialSync::new(client.clone(), Store::open_in_memory().unwrap(), config());
    let (result, _) = run(&mut sync, &CancellationToken::new()).await;
    let err = result.unwrap_err();
    assert!(
        matches!(err, Error::Gmail(api::Error::Api { .. })),
        "{err:?}"
    );
    assert!(!err.is_transient());

    let store = sync.into_store();
    let state = store.sync_state().unwrap();
    assert!(!state.backfill_complete);
    assert_eq!(state.history_id, Some(HistoryId::new(PROFILE_HISTORY_ID)));
    let cp = checkpoint(&store);
    assert_eq!(cp["phase"], "backfill");
    assert_eq!(cp["page_token"], "b2");
    assert!(stored(&store, "t4"));
    assert!(!stored(&store, "t5"));

    // A new run continues from page b2: the profile and earlier pages are not
    // requested again (the mocks expect one call each).
    let mut sync = InitialSync::new(client, store, config());
    let (result, seen) = run(&mut sync, &CancellationToken::new()).await;
    let done = result.unwrap();
    assert!(done.resumed);
    assert_eq!(done.threads, 1);
    assert_eq!(phases(&seen), [Phase::Labels, Phase::Backfill, Phase::Done]);
    assert!(stored(sync.store(), "t5"));
    let state = sync.store().sync_state().unwrap();
    assert!(state.backfill_complete);
    assert_eq!(state.history_id, Some(HistoryId::new(PROFILE_HISTORY_ID)));
    assert_eq!(batch.requested("threads"), ["t1", "t2", "t3", "t4", "t5"]);
}

/// `mock` matching only with `q` as given (`None`: no `q` at all).
fn with_q(mock: wiremock::MockBuilder, q: Option<&str>) -> wiremock::MockBuilder {
    match q {
        Some(q) => mock.and(query_param("q", q)),
        None => mock.and(query_param_is_missing("q")),
    }
}

/// Mounts profile, labels, an Inbox page and a backfill page that only match
/// with `q` as given (`None`: no `q` at all).
async fn mount_cutoff(server: &MockServer, q: Option<&str>) {
    let mailbox = mailbox();
    mount_profile(server, 1).await;
    mount_labels(server).await;
    with_q(list_page(Listing::Inbox, None), q)
        .respond_with(page(&mailbox, &["t1"], None))
        .expect(1)
        .mount(server)
        .await;
    with_q(list_page(Listing::Backfill, None), q)
        .respond_with(page(&mailbox, &["t4"], None))
        .expect(1)
        .mount(server)
        .await;
    BatchResponder::new(mailbox).mount(server).await;
}

#[tokio::test]
async fn inbox_and_backfill_stop_at_the_cutoff_date() {
    let since = datetime!(2026-08-01 0:00 UTC);
    let (server, client) = setup().await;
    mount_cutoff(&server, Some(&format!("after:{}", since.unix_timestamp()))).await;

    let config = InitialSyncConfig {
        cutoff: Cutoff::Since(since),
        ..config()
    };
    let mut sync = InitialSync::new(client, Store::open_in_memory().unwrap(), config);
    let done = sync.run(&CancellationToken::new(), |_| {}).await.unwrap();
    assert_eq!(done.threads, 2);
    assert!(stored(sync.store(), "t1"));
    assert!(stored(sync.store(), "t4"));
}

#[tokio::test]
async fn the_cutoff_defaults_to_the_last_90_days() {
    let (server, client) = setup().await;
    mount_profile(&server, 1).await;
    mount_labels(&server).await;
    list_page(Listing::Inbox, None)
        .respond_with(page(&mailbox(), &[], None))
        .mount(&server)
        .await;
    list_page(Listing::Backfill, None)
        .respond_with(page(&mailbox(), &[], None))
        .expect(1)
        .mount(&server)
        .await;

    let mut sync = InitialSync::new(
        client,
        Store::open_in_memory().unwrap(),
        InitialSyncConfig::default(),
    );
    sync.run(&CancellationToken::new(), |_| {}).await.unwrap();

    let expected = (OffsetDateTime::now_utc() - time::Duration::days(90)).unix_timestamp();
    let requests = server.received_requests().await.unwrap();
    // The Inbox and backfill listings both carry the same cutoff.
    let queries: Vec<String> = requests
        .iter()
        .filter(|r| r.url.path().ends_with("/threads"))
        .map(|r| {
            r.url
                .query_pairs()
                .find(|(k, _)| k == "q")
                .map(|(_, v)| v.into_owned())
                .expect("a cutoff query")
        })
        .collect();
    assert_eq!(queries.len(), 2);
    assert_eq!(queries[0], queries[1]);
    let after: i64 = queries[0].strip_prefix("after:").unwrap().parse().unwrap();
    assert!((after - expected).abs() < 60, "{queries:?} vs {expected}");
}

#[tokio::test]
async fn cutoff_all_lists_without_a_query() {
    let (server, client) = setup().await;
    mount_cutoff(&server, None).await;
    let mut sync = InitialSync::new(client, Store::open_in_memory().unwrap(), config());
    let done = sync.run(&CancellationToken::new(), |_| {}).await.unwrap();
    assert_eq!(done.threads, 2);
}

#[tokio::test]
async fn an_old_inbox_checkpoint_restarts_the_inbox_with_the_cutoff() {
    let (server, client) = setup().await;
    // A sync started before the Inbox used the cutoff, stopped mid-Inbox.
    let store = Store::open_in_memory().unwrap();
    let mut state = store.sync_state().unwrap();
    state.history_id = Some(HistoryId::new(4000));
    state.backfill_cursor = Some(
        r#"{"phase":"inbox","page_token":"old-unbounded-token","after":1785542400}"#.to_owned(),
    );
    store.set_sync_state(&state).unwrap();
    // No profile call: the sync resumes. Both listings start from their first
    // page with the cutoff.
    mount_profile(&server, 0).await;
    let mailbox = mailbox();
    mount_labels(&server).await;
    with_q(list_page(Listing::Inbox, None), Some("after:1785542400"))
        .respond_with(page(&mailbox, &["t1"], None))
        .expect(1)
        .mount(&server)
        .await;
    with_q(list_page(Listing::Backfill, None), Some("after:1785542400"))
        .respond_with(page(&mailbox, &[], None))
        .expect(1)
        .mount(&server)
        .await;
    BatchResponder::new(mailbox).mount(&server).await;

    let mut sync = InitialSync::new(client, store, config());
    let done = sync.run(&CancellationToken::new(), |_| {}).await.unwrap();
    assert!(done.resumed);
    assert_eq!(done.threads, 1);
    let state = sync.store().sync_state().unwrap();
    assert!(state.backfill_complete);
    assert_eq!(state.history_id, Some(HistoryId::new(4000)));
}

#[tokio::test]
async fn cancelling_between_pages_keeps_the_checkpoint() {
    let (server, client) = setup().await;
    let batch = mount_all(&server).await;
    let cancel = CancellationToken::new();

    let mut sync = InitialSync::new(client.clone(), Store::open_in_memory().unwrap(), config());
    let result = sync
        .run(&cancel, |p| {
            if p.phase == Phase::Inbox && p.threads == 2 {
                cancel.cancel();
            }
        })
        .await;
    assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");

    let store = sync.into_store();
    let cp = checkpoint(&store);
    assert_eq!(cp["phase"], "inbox");
    assert_eq!(cp["page_token"], "i2");
    assert!(stored(&store, "t2"));
    assert!(!stored(&store, "t3"));

    // Resuming with a fresh token finishes without re-listing the first page.
    let mut sync = InitialSync::new(client, store, config());
    let done = sync.run(&CancellationToken::new(), |_| {}).await.unwrap();
    assert!(done.resumed);
    assert_eq!(done.threads, 3);
    assert!(sync.store().sync_state().unwrap().backfill_complete);
    assert_eq!(batch.requested("threads"), ["t1", "t2", "t3", "t4", "t5"]);
}

#[tokio::test]
async fn cancelling_aborts_a_request_in_flight() {
    let (server, client) = setup().await;
    mount_profile(&server, 1).await;
    mount_labels(&server).await;
    list_page(Listing::Inbox, None)
        .respond_with(page(&mailbox(), &["t1"], None).set_delay(Duration::from_secs(30)))
        .mount(&server)
        .await;

    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        trigger.cancel();
    });

    let mut sync = InitialSync::new(client, Store::open_in_memory().unwrap(), config());
    let started = Instant::now();
    let result = sync.run(&cancel, |_| {}).await;
    assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");
    assert!(started.elapsed() < Duration::from_secs(10));

    // The sync had started: its history ID and first checkpoint are saved.
    let store = sync.store();
    assert_eq!(
        store.sync_state().unwrap().history_id,
        Some(HistoryId::new(PROFILE_HISTORY_ID))
    );
    assert_eq!(checkpoint(store)["phase"], "inbox");
    assert!(checkpoint(store).get("page_token").is_none());
}

#[tokio::test]
async fn prefetches_the_newest_inbox_bodies() {
    let (server, client) = setup().await;
    let batch = mount_all(&server).await;
    let config = InitialSyncConfig {
        prefetch_inbox_bodies: Some(2),
        ..config()
    };
    let mut sync = InitialSync::new(client, Store::open_in_memory().unwrap(), config);

    let (result, seen) = run(&mut sync, &CancellationToken::new()).await;
    let done = result.unwrap();
    assert_eq!(done.bodies, 2);
    assert_eq!(
        phases(&seen),
        [
            Phase::Labels,
            Phase::Inbox,
            Phase::Prefetch,
            Phase::Backfill,
            Phase::Done
        ]
    );

    // m1 and m2 are the newest Inbox messages.
    assert_eq!(batch.requested("messages"), ["m1", "m2"]);
    let raw = batch
        .calls()
        .into_iter()
        .flatten()
        .filter(|i| i.target.contains("/messages/"))
        .all(|i| i.target.ends_with("?format=raw"));
    assert!(raw);
    let store = sync.store();
    let body = store.body(&MessageId::new("m1")).unwrap().unwrap();
    assert_eq!(body.text.as_deref().map(str::trim_end), Some("Body of m1"));
    assert_eq!(body.html, None);
    assert!(store.body(&MessageId::new("m3")).unwrap().is_none());
}

#[tokio::test]
async fn threads_deleted_before_their_fetch_are_skipped() {
    let (server, client) = setup().await;
    let full = mailbox();
    // t2 is listed but gone by the time it is fetched.
    let mut fetched = full.clone();
    fetched.threads.remove("t2");
    mount_profile(&server, 1).await;
    mount_labels(&server).await;
    list_page(Listing::Inbox, None)
        .respond_with(page(&full, &["t1", "t2"], None))
        .mount(&server)
        .await;
    list_page(Listing::Backfill, None)
        .respond_with(page(&full, &[], None))
        .mount(&server)
        .await;
    BatchResponder::new(fetched).mount(&server).await;

    let mut sync = InitialSync::new(client, Store::open_in_memory().unwrap(), config());
    let done = sync.run(&CancellationToken::new(), |_| {}).await.unwrap();
    assert_eq!(done.threads, 1);
    assert_eq!(done.skipped_threads, 1);
    assert!(stored(sync.store(), "t1"));
    assert!(!stored(sync.store(), "t2"));
}

#[tokio::test]
async fn the_sync_future_is_send() {
    fn assert_send<T: Send>(_: T) {}
    let (_server, client) = setup().await;
    let mut sync = InitialSync::new(client, Store::open_in_memory().unwrap(), config());
    assert_send(sync.run(&CancellationToken::new(), |_| {}));
}

/// Gmail's per-user rate-limit response.
fn rate_limited() -> ResponseTemplate {
    ResponseTemplate::new(429).set_body_json(serde_json::json!({
        "error": {
            "code": 429,
            "message": "Quota exceeded for quota metric 'Total Query Cost'",
            "errors": [{"reason": "rateLimitExceeded"}]
        }
    }))
}

/// How many times the sync reported pausing for the rate limit.
fn pauses(seen: &[Progress]) -> usize {
    seen.iter().filter(|p| p.rate_limit_wait.is_some()).count()
}

#[tokio::test]
async fn rate_limits_mid_backfill_pause_and_resume_in_the_same_run() {
    let (server, client) = setup().await;
    // The second backfill page is rate-limited three times, then works.
    list_page(Listing::Backfill, Some("b2"))
        .respond_with(rate_limited())
        .up_to_n_times(3)
        .with_priority(1)
        .mount(&server)
        .await;
    // Every mock in mount_all expects exactly one call, so nothing before the
    // checkpoint is fetched again.
    let batch = mount_all(&server).await;

    let mut sync = InitialSync::new(client, Store::open_in_memory().unwrap(), config());
    let (result, seen) = run(&mut sync, &CancellationToken::new()).await;
    let done = result.unwrap();

    assert_eq!(pauses(&seen), 3);
    // The client does not retry in these tests, so each 429 surfaced.
    assert_eq!(done.rate_limited_requests, 3);
    assert_eq!(done.retried_requests, 0);
    assert!(seen.iter().all(
        |p| p.rate_limit_wait.is_none() || p.rate_limit_wait == Some(Duration::from_millis(10))
    ));
    assert_eq!(done.rate_limit_wait, None);
    assert!(!done.resumed);
    assert_eq!(done.threads, 5);
    assert_eq!(done.phase, Phase::Done);
    let state = sync.store().sync_state().unwrap();
    assert!(state.backfill_complete);
    assert_eq!(state.history_id, Some(HistoryId::new(PROFILE_HISTORY_ID)));
    assert_eq!(batch.requested("threads"), ["t1", "t2", "t3", "t4", "t5"]);
}

#[tokio::test]
async fn rate_limited_batches_pause_and_resume() {
    let (server, client) = setup().await;
    let mailbox = mailbox();
    mount_profile(&server, 1).await;
    mount_labels(&server).await;
    list_page(Listing::Inbox, None)
        .respond_with(page(&mailbox, &["t1", "t2"], None))
        .mount(&server)
        .await;
    list_page(Listing::Backfill, None)
        .respond_with(page(&mailbox, &["t4"], None))
        .mount(&server)
        .await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path(common::BATCH))
        .respond_with(rate_limited())
        .up_to_n_times(2)
        .with_priority(1)
        .mount(&server)
        .await;
    BatchResponder::new(mailbox).mount(&server).await;

    let mut sync = InitialSync::new(client, Store::open_in_memory().unwrap(), config());
    let (result, seen) = run(&mut sync, &CancellationToken::new()).await;
    let done = result.unwrap();
    assert_eq!(pauses(&seen), 2);
    assert_eq!(done.threads, 3);
    assert!(sync.store().sync_state().unwrap().backfill_complete);
}

#[tokio::test]
async fn persistent_rate_limits_give_up_after_the_configured_pauses() {
    let (server, client) = setup().await;
    list_page(Listing::Backfill, Some("b2"))
        .respond_with(rate_limited())
        .with_priority(1)
        .mount(&server)
        .await;
    let mailbox = mailbox();
    mount_profile(&server, 1).await;
    mount_labels(&server).await;
    mount_inbox(&server, &mailbox).await;
    list_page(Listing::Backfill, None)
        .respond_with(page(&mailbox, &["t3", "t4"], Some("b2")))
        .expect(1)
        .mount(&server)
        .await;
    BatchResponder::new(mailbox).mount(&server).await;

    let config = InitialSyncConfig {
        max_rate_limit_pauses: 2,
        ..config()
    };
    let mut sync = InitialSync::new(client, Store::open_in_memory().unwrap(), config);
    let (result, seen) = run(&mut sync, &CancellationToken::new()).await;
    let err = result.unwrap_err();
    assert!(err.rate_limit_retry_after().is_some(), "{err:?}");
    assert!(err.is_transient());
    assert_eq!(pauses(&seen), 2);
    let cp = checkpoint(sync.store());
    assert_eq!(cp["phase"], "backfill");
    assert_eq!(cp["page_token"], "b2");
}

#[tokio::test]
async fn cancelling_during_a_rate_limit_pause_stops_the_sync() {
    let (server, client) = setup().await;
    mount_profile(&server, 1).await;
    mount_labels(&server).await;
    list_page(Listing::Inbox, None)
        .respond_with(rate_limited())
        .mount(&server)
        .await;

    let cancel = CancellationToken::new();
    let config = InitialSyncConfig {
        rate_limit_pause: Duration::from_secs(60),
        ..config()
    };
    let mut sync = InitialSync::new(client, Store::open_in_memory().unwrap(), config);
    let started = Instant::now();
    let result = sync
        .run(&cancel, |p| {
            if p.rate_limit_wait.is_some() {
                cancel.cancel();
            }
        })
        .await;
    assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[tokio::test]
async fn rate_limited_batch_items_are_retried_and_counted() {
    let (server, client) = setup_retrying().await;
    let mailbox = mailbox();
    mount_profile(&server, 1).await;
    mount_labels(&server).await;
    mount_inbox(&server, &mailbox).await;
    mount_backfill(&server, &mailbox).await;
    // Gmail turns away some items of a busy batch; they are resent on their own.
    let batch = BatchResponder::new(mailbox).rate_limit_once(&["t2", "t4", "t5"]);
    batch.mount(&server).await;

    let mut sync = InitialSync::new(client, Store::open_in_memory().unwrap(), config());
    let (result, seen) = run(&mut sync, &CancellationToken::new()).await;
    let done = result.unwrap();

    assert_eq!(done.threads, 5);
    assert_eq!(done.retried_requests, 3);
    assert_eq!(done.rate_limited_requests, 3);
    // The client's retries absorbed them: the sync never paused.
    assert_eq!(pauses(&seen), 0);
    for t in ["t1", "t2", "t3", "t4", "t5"] {
        assert!(stored(sync.store(), t), "{t}");
    }
    let mut requested = batch.requested("threads");
    requested.sort();
    assert_eq!(requested, ["t1", "t2", "t2", "t3", "t4", "t4", "t5", "t5"]);
}

/// A store holding a full sync of `mailbox()` plus `extra` threads, as left
/// by an earlier sync, then reset for a full resync.
async fn synced_then_reset(extra: Mailbox) -> Store {
    let (server, client) = setup().await;
    mount_all(&server).await;
    let mut sync = InitialSync::new(client, Store::open_in_memory().unwrap(), config());
    sync.run(&CancellationToken::new(), |_| {}).await.unwrap();
    let store = sync.into_store();
    for (id, thread) in &extra.threads {
        let (mid, labels, date) = &thread.messages[0];
        let date =
            OffsetDateTime::from_unix_timestamp_nanos(i128::from(*date) * 1_000_000).unwrap();
        let t = openmail_core::Thread {
            id: ThreadId::new(id),
            history_id: HistoryId::new(thread.history_id),
            subject: None,
            snippet: String::new(),
            last_message_date: date,
            message_count: 1,
            label_ids: Vec::new(),
            senders: Vec::new(),
            has_attachments: false,
        };
        let m = openmail_core::Message {
            id: MessageId::new(mid),
            thread_id: ThreadId::new(id),
            history_id: HistoryId::new(thread.history_id),
            label_ids: labels.iter().map(|l| LabelId::new(l.as_str())).collect(),
            snippet: String::new(),
            internal_date: date,
            size_estimate: 1,
            headers: openmail_core::MessageHeaders::default(),
            has_attachments: false,
        };
        store.replace_threads(&[(t, vec![m])]).unwrap();
    }
    InitialSync::reset(&store).unwrap();
    store
}

#[tokio::test]
async fn reset_clears_the_history_id_and_checkpoint_but_keeps_rows() {
    let store = synced_then_reset(Mailbox::default()).await;
    let state = store.sync_state().unwrap();
    assert_eq!(state.history_id, None);
    assert_eq!(state.backfill_cursor, None);
    assert!(!state.backfill_complete);
    assert!(state.last_full_sync.is_some());
    assert!(stored(&store, "t1"));
}

#[tokio::test]
async fn a_full_resync_refetches_nothing_unchanged_and_drops_vanished_threads() {
    // t8 vanished from the server; t400 is older than the resync's cutoff.
    let store = synced_then_reset(Mailbox::default().with(8, &["INBOX"]).with(400, &[])).await;
    let since = datetime!(2026-01-01 0:00 UTC);
    let q = format!("after:{}", since.unix_timestamp());
    let (server, client) = setup().await;
    let mailbox = mailbox();
    mount_profile(&server, 1).await;
    mount_labels(&server).await;
    with_q(list_page(Listing::Inbox, None), Some(&q))
        .respond_with(page(&mailbox, &["t1", "t2", "t3"], None))
        .mount(&server)
        .await;
    with_q(list_page(Listing::Backfill, None), Some(&q))
        .respond_with(page(&mailbox, &["t3", "t4", "t5"], None))
        .mount(&server)
        .await;
    let batch = BatchResponder::new(mailbox);
    batch.mount(&server).await;

    let config = InitialSyncConfig {
        cutoff: Cutoff::Since(since),
        ..config()
    };
    let mut sync = InitialSync::new(client, store, config);
    let done = sync.run(&CancellationToken::new(), |_| {}).await.unwrap();

    assert_eq!(done.threads, 0);
    assert_eq!(done.skipped_threads, 6);
    assert_eq!(done.deleted_threads, 1);
    assert!(batch.calls().is_empty());
    let store = sync.store();
    assert!(!stored(store, "t8"));
    assert!(stored(store, "t400"));
    for t in ["t1", "t2", "t3", "t4", "t5"] {
        assert!(stored(store, t), "{t}");
    }
    let state = store.sync_state().unwrap();
    assert_eq!(state.history_id, Some(HistoryId::new(PROFILE_HISTORY_ID)));
    assert!(state.backfill_complete);
}

#[tokio::test]
async fn a_stored_thread_gone_when_fetched_is_deleted() {
    let (server, client) = setup().await;
    let mut changed = mailbox();
    changed.threads.get_mut("t2").unwrap().history_id += 1;
    let mut fetched = changed.clone();
    fetched.threads.remove("t2");
    mount_profile(&server, 1).await;
    mount_labels(&server).await;
    list_page(Listing::Inbox, None)
        .respond_with(page(&changed, &["t2"], None))
        .mount(&server)
        .await;
    list_page(Listing::Backfill, None)
        .respond_with(page(&changed, &[], None))
        .mount(&server)
        .await;
    BatchResponder::new(fetched).mount(&server).await;

    let store = synced_then_reset(Mailbox::default()).await;
    let mut sync = InitialSync::new(client, store, config());
    let done = sync.run(&CancellationToken::new(), |_| {}).await.unwrap();
    assert!(!stored(sync.store(), "t2"));
    // t2 when fetched; t1, t3, t4 and t5 at the end, since Cutoff::All
    // covers them and this listing did not include them.
    assert_eq!(done.deleted_threads, 5);
}

#[tokio::test]
async fn a_rate_limit_burst_cuts_the_quota_rate_without_pausing_the_sync() {
    let (server, client) = setup_retrying().await;
    let mailbox = mailbox();
    mount_profile(&server, 1).await;
    mount_labels(&server).await;
    mount_inbox(&server, &mailbox).await;
    mount_backfill(&server, &mailbox).await;
    // A burst of rate limits: every thread is turned away once.
    BatchResponder::new(mailbox)
        .rate_limit_once(&["t1", "t2", "t3", "t4", "t5"])
        .mount(&server)
        .await;

    let start = client.stats().units_per_second.unwrap();
    let config = InitialSyncConfig {
        rate_limit_pause: Duration::from_secs(60),
        ..config()
    };
    let mut sync = InitialSync::new(client, Store::open_in_memory().unwrap(), config);
    let started = Instant::now();
    let (result, seen) = run(&mut sync, &CancellationToken::new()).await;
    let done = result.unwrap();

    assert_eq!(done.threads, 5);
    assert_eq!(done.rate_limited_requests, 5);
    // The client slowed down and retried; the 60 s sync-level pause was never
    // needed.
    assert_eq!(pauses(&seen), 0);
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "{:?}",
        started.elapsed()
    );
    let rate = done.units_per_second.unwrap();
    assert!(rate < start, "{rate} vs {start}");
}
