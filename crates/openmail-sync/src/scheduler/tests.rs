//! The scheduling loop on a paused tokio clock, with a scripted engine.

use std::collections::VecDeque;
use std::sync::Mutex;

use openmail_gmail::api::{QuotaKind, RetryPolicy, TokenFuture, TokenProvider};

use super::*;

/// What the fake engine was asked to do, and when (seconds since the start).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Call {
    Initial(u64),
    Poll(u64),
}

/// Answers polls from a script (then with no changes), and records calls.
struct Fake {
    start: Instant,
    initial_complete: bool,
    polls: VecDeque<Result<Changes>>,
    calls: Vec<Call>,
}

impl Fake {
    fn new(polls: impl IntoIterator<Item = Result<Changes>>) -> Self {
        Self {
            start: Instant::now(),
            initial_complete: true,
            polls: polls.into_iter().collect(),
            calls: Vec::new(),
        }
    }

    fn now(&self) -> u64 {
        (Instant::now() - self.start).as_secs()
    }

    fn poll_times(&self) -> Vec<u64> {
        self.calls
            .iter()
            .filter_map(|c| match c {
                Call::Poll(at) => Some(*at),
                Call::Initial(_) => None,
            })
            .collect()
    }
}

impl Engine for Fake {
    fn initial_complete(&mut self) -> Result<bool> {
        Ok(self.initial_complete)
    }

    async fn run_initial(
        &mut self,
        _cancel: &CancellationToken,
        mut progress: impl FnMut(&Progress) + Send,
    ) -> Result<Progress> {
        self.calls.push(Call::Initial(self.now()));
        let done = Progress {
            phase: crate::Phase::Done,
            threads: 3,
            ..Progress::default()
        };
        progress(&done);
        self.initial_complete = true;
        Ok(done)
    }

    async fn poll(&mut self, _cancel: &CancellationToken) -> Result<Changes> {
        self.calls.push(Call::Poll(self.now()));
        let result = self
            .polls
            .pop_front()
            .unwrap_or_else(|| Ok(Changes::default()));
        if matches!(&result, Ok(changes) if changes.full_resync) {
            self.initial_complete = false;
        }
        result
    }
}

fn config() -> SchedulerConfig {
    SchedulerConfig {
        active_interval: Duration::from_secs(30),
        idle_interval: Duration::from_secs(180),
        min_backoff: Duration::from_secs(5),
        max_backoff: Duration::from_secs(60),
    }
}

/// Drives `fake` until `secs` seconds have passed, while `script` runs
/// alongside with the scheduler's handle. Returns the result and the events.
async fn drive_for<F: Future<Output = ()>>(
    fake: &mut Fake,
    config: SchedulerConfig,
    secs: u64,
    script: impl FnOnce(SchedulerHandle) -> F,
) -> (Result<()>, Vec<SyncEvent>) {
    let (commands, mut receiver) = mpsc::unbounded_channel();
    let handle = SchedulerHandle { commands };
    let cancel = CancellationToken::new();
    let events = Mutex::new(Vec::new());
    let stop = async {
        tokio::time::sleep(Duration::from_secs(secs)).await;
        cancel.cancel();
    };
    let driver = drive(fake, &config, &mut receiver, &cancel, |event| {
        events.lock().unwrap().push(event);
    });
    let (result, (), ()) = tokio::join!(driver, stop, script(handle));
    (result, events.into_inner().unwrap())
}

async fn nothing(_: SchedulerHandle) {}

/// Sleeps until `secs` seconds after the test started (the clock is paused,
/// so this is exact).
async fn at(start: Instant, secs: u64) {
    tokio::time::sleep_until(start + Duration::from_secs(secs)).await;
}

fn rate_limit(retry_after: Option<u64>) -> Error {
    Error::Gmail(api::Error::Quota {
        kind: QuotaKind::RateLimit,
        message: String::new(),
        retry_after: retry_after.map(Duration::from_secs),
    })
}

struct NoToken;

impl TokenProvider for NoToken {
    fn access_token(&self) -> TokenFuture<'_> {
        Box::pin(async { Ok("token".to_owned()) })
    }

    fn refresh_access_token<'a>(&'a self, _rejected: &'a str) -> TokenFuture<'a> {
        Box::pin(async { Ok("token".to_owned()) })
    }
}

/// Real "could not connect" errors, made before the clock is paused.
async fn connection_errors(n: usize) -> Vec<Error> {
    let client = GmailClient::builder(Arc::new(NoToken))
        .base_url("http://127.0.0.1:1/".parse().unwrap())
        .retry_policy(RetryPolicy::none())
        .quota_limit(None)
        .build()
        .unwrap();
    let mut errors = Vec::new();
    for _ in 0..n {
        errors.push(client.get_profile().await.unwrap_err().into());
    }
    errors
}

fn connectivity(events: &[SyncEvent]) -> Vec<Connectivity> {
    events
        .iter()
        .filter_map(|e| match e {
            SyncEvent::Connectivity(c) => Some(*c),
            _ => None,
        })
        .collect()
}

fn retry_waits(events: &[SyncEvent]) -> Vec<u64> {
    events
        .iter()
        .filter_map(|e| match e {
            SyncEvent::Failed { retry_in, .. } => Some(retry_in.as_secs()),
            _ => None,
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn polls_on_the_active_interval() {
    let mut fake = Fake::new([]);
    let (result, events) = drive_for(&mut fake, config(), 95, nothing).await;
    assert!(result.is_ok());
    assert_eq!(fake.poll_times(), [0, 30, 60, 90]);
    let polled = events
        .iter()
        .filter(|e| matches!(e, SyncEvent::Polled(_)))
        .count();
    assert_eq!(polled, 4);
    assert_eq!(connectivity(&events), [Connectivity::Online]);
}

#[tokio::test(start_paused = true)]
async fn idle_polls_less_often_and_becoming_active_reschedules() {
    let mut fake = Fake::new([]);
    let start = Instant::now();
    let _ = drive_for(&mut fake, config(), 250, |handle| async move {
        at(start, 10).await;
        handle.set_activity(Activity::Idle);
        at(start, 200).await;
        // The active deadline (180 + 30) is still ahead.
        handle.set_activity(Activity::Active);
    })
    .await;
    assert_eq!(fake.poll_times(), [0, 180, 210, 240]);
}

#[tokio::test(start_paused = true)]
async fn switching_to_active_after_its_deadline_polls_at_once() {
    let mut fake = Fake::new([]);
    let start = Instant::now();
    let _ = drive_for(&mut fake, config(), 120, |handle| async move {
        at(start, 1).await;
        handle.set_activity(Activity::Idle);
        at(start, 100).await;
        handle.set_activity(Activity::Active);
    })
    .await;
    assert_eq!(fake.poll_times(), [0, 100]);
}

#[tokio::test(start_paused = true)]
async fn poll_now_polls_immediately() {
    let mut fake = Fake::new([]);
    let start = Instant::now();
    let _ = drive_for(&mut fake, config(), 40, |handle| async move {
        at(start, 5).await;
        handle.poll_now();
    })
    .await;
    assert_eq!(fake.poll_times(), [0, 5, 35]);
}

#[tokio::test]
async fn backs_off_exponentially_while_offline() {
    let errors = connection_errors(4).await;
    tokio::time::pause();
    let mut fake = Fake::new(errors.into_iter().map(Err));
    let (result, events) = drive_for(&mut fake, config(), 100, nothing).await;
    assert!(result.is_ok());
    // 5, 10, 20, 40 s, then back to normal.
    assert_eq!(fake.poll_times(), [0, 5, 15, 35, 75]);
    assert_eq!(retry_waits(&events), [5, 10, 20, 40]);
    assert_eq!(
        connectivity(&events),
        [Connectivity::Offline, Connectivity::Online]
    );
}

#[tokio::test(start_paused = true)]
async fn backoff_is_capped_and_poll_now_cuts_it_short() {
    let mut fake = Fake::new((0..7).map(|_| Err(rate_limit(None))));
    let start = Instant::now();
    let (_, events) = drive_for(&mut fake, config(), 200, |handle| async move {
        at(start, 160).await;
        handle.poll_now();
    })
    .await;
    // 5, 10, 20, 40, then the 60 s cap; at 160 a poll on demand, which
    // fails again and waits 60 s.
    assert_eq!(fake.poll_times(), [0, 5, 15, 35, 75, 135, 160]);
    assert_eq!(retry_waits(&events), [5, 10, 20, 40, 60, 60, 60]);
    // Rate limits come from Gmail, so it is online throughout.
    assert_eq!(connectivity(&events), [Connectivity::Online]);
}

#[tokio::test(start_paused = true)]
async fn a_rate_limit_waits_at_least_its_retry_after() {
    let mut fake = Fake::new([Err(rate_limit(Some(45)))]);
    let _ = drive_for(&mut fake, config(), 60, nothing).await;
    assert_eq!(fake.poll_times(), [0, 45]);
}

#[tokio::test(start_paused = true)]
async fn runs_the_initial_sync_first() {
    let mut fake = Fake::new([]);
    fake.initial_complete = false;
    let (_, events) = drive_for(&mut fake, config(), 31, nothing).await;
    assert_eq!(
        fake.calls,
        [Call::Initial(0), Call::Poll(0), Call::Poll(30)]
    );
    assert!(matches!(&events[0], SyncEvent::Initial(p) if p.threads == 3));
}

#[tokio::test(start_paused = true)]
async fn an_expired_history_resyncs_at_once() {
    let resync = Changes {
        full_resync: true,
        ..Changes::default()
    };
    let mut fake = Fake::new([Ok(Changes::default()), Ok(resync.clone()), Ok(resync)]);
    let _ = drive_for(&mut fake, config(), 61, nothing).await;
    // At 30 the history has expired: resync and poll straight away. The poll
    // after it reports another expiry, which waits for the next interval
    // instead of looping.
    assert_eq!(
        fake.calls,
        [
            Call::Poll(0),
            Call::Poll(30),
            Call::Initial(30),
            Call::Poll(30),
            Call::Initial(60),
            Call::Poll(60),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn a_non_transient_error_stops_the_scheduler() {
    let mut fake = Fake::new([Ok(Changes::default()), Err(api::Error::Unauthorized.into())]);
    let (result, _) = drive_for(&mut fake, config(), 100, nothing).await;
    assert!(matches!(
        result,
        Err(Error::Gmail(api::Error::Unauthorized))
    ));
    assert_eq!(fake.poll_times(), [0, 30]);
}

#[tokio::test(start_paused = true)]
async fn cancelling_mid_attempt_stops_cleanly() {
    let mut fake = Fake::new([Err(Error::Cancelled)]);
    let (result, events) = drive_for(&mut fake, config(), 100, nothing).await;
    assert!(result.is_ok());
    assert_eq!(fake.poll_times(), [0]);
    assert!(events.is_empty());
}

#[test]
fn backoff_doubles_up_to_the_cap() {
    let config = config();
    let waits: Vec<u64> = (1..=7).map(|n| config.backoff(n).as_secs()).collect();
    assert_eq!(waits, [5, 10, 20, 40, 60, 60, 60]);
    assert_eq!(config.backoff(u32::MAX), Duration::from_secs(60));
}

#[test]
fn the_scheduler_future_is_send() {
    fn assert_send<T: Send>(_: T) {}
    let client = GmailClient::new(Arc::new(NoToken)).unwrap();
    let mut scheduler = Scheduler::new(
        client,
        Store::open_in_memory().unwrap(),
        InitialSyncConfig::default(),
        SchedulerConfig::default(),
    );
    let cancel = CancellationToken::new();
    assert_send(scheduler.run(&cancel, |_| {}));
}
