//! The polling scheduler: runs the initial sync if needed, then polls
//! `history.list` on an interval, backing off when offline or failing.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use openmail_gmail::GmailClient;
use openmail_gmail::api;
use openmail_store::Store;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::{Changes, Error, HistorySync, InitialSync, InitialSyncConfig, Progress, Result};

/// How often the scheduler polls, and how it backs off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedulerConfig {
    /// Between polls while the app is in use (window focused).
    pub active_interval: Duration,
    /// Between polls while it is idle or in the background.
    pub idle_interval: Duration,
    /// The wait after the first failure in a row; it doubles with each
    /// further failure.
    pub min_backoff: Duration,
    /// The longest wait after failures.
    pub max_backoff: Duration,
}

impl Default for SchedulerConfig {
    /// Every 30 s while active and 3 min while idle; backoff from 5 s to 5 min.
    fn default() -> Self {
        Self {
            active_interval: Duration::from_secs(30),
            idle_interval: Duration::from_secs(180),
            min_backoff: Duration::from_secs(5),
            max_backoff: Duration::from_secs(300),
        }
    }
}

impl SchedulerConfig {
    fn interval(&self, activity: Activity) -> Duration {
        match activity {
            Activity::Active => self.active_interval,
            Activity::Idle => self.idle_interval,
        }
    }

    /// The wait after `failures` failures in a row (at least 1).
    fn backoff(&self, failures: u32) -> Duration {
        let doublings = failures.saturating_sub(1).min(16);
        self.min_backoff
            .saturating_mul(1 << doublings)
            .min(self.max_backoff)
    }
}

/// Whether the app is in use, which sets the poll interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Activity {
    /// In use: poll every [`SchedulerConfig::active_interval`].
    #[default]
    Active,
    /// Idle or in the background: poll every
    /// [`SchedulerConfig::idle_interval`].
    Idle,
}

/// Whether Gmail can be reached, judged from the last request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connectivity {
    Online,
    /// The last request failed without reaching Gmail (no connection, DNS
    /// failure, timeout).
    Offline,
}

/// What the scheduler reports as it goes.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum SyncEvent {
    /// The initial sync (or a full resync) made progress.
    Initial(Progress),
    /// A poll of the mailbox's history finished (its changes may be empty).
    Polled(Changes),
    /// Connectivity changed. Reported first after the first attempt.
    Connectivity(Connectivity),
    /// A sync attempt failed with a transient error; the next one starts in
    /// `retry_in` (or sooner on [`SchedulerHandle::poll_now`]).
    Failed {
        error: Arc<Error>,
        retry_in: Duration,
    },
}

#[derive(Debug, Clone, Copy)]
enum Command {
    PollNow,
    SetActivity(Activity),
}

/// Controls a running [`Scheduler`] from elsewhere, such as the UI. Cheap to
/// clone.
#[derive(Debug, Clone)]
pub struct SchedulerHandle {
    commands: mpsc::UnboundedSender<Command>,
}

impl SchedulerHandle {
    /// Polls now (for example when the window gains focus), even while
    /// backing off after failures.
    pub fn poll_now(&self) {
        let _ = self.commands.send(Command::PollNow);
    }

    /// Switches between the active and idle poll intervals. The next poll is
    /// rescheduled from the last one.
    pub fn set_activity(&self, activity: Activity) {
        let _ = self.commands.send(Command::SetActivity(activity));
    }
}

/// Keeps an account's store in sync: runs (or resumes) [`InitialSync`] until
/// it is complete, then polls [`HistorySync`] every
/// [`SchedulerConfig::active_interval`] or
/// [`idle_interval`](SchedulerConfig::idle_interval).
///
/// Transient failures (network errors, rate limits, Gmail server errors) are
/// retried with exponential backoff, and a failure to reach Gmail at all
/// marks the scheduler [offline](Connectivity::Offline) until a request
/// succeeds. If Gmail's history has expired, the store is reset and the
/// initial sync runs again at once. Other errors (signing in, an exhausted
/// daily quota, the store) stop the scheduler.
#[derive(Debug)]
pub struct Scheduler {
    engine: StoreEngine,
    config: SchedulerConfig,
    commands: mpsc::UnboundedSender<Command>,
    receiver: mpsc::UnboundedReceiver<Command>,
}

impl Scheduler {
    /// A scheduler for the account `client` is signed in to, syncing into
    /// `store`.
    pub fn new(
        client: GmailClient,
        store: Store,
        initial: InitialSyncConfig,
        config: SchedulerConfig,
    ) -> Self {
        let (commands, receiver) = mpsc::unbounded_channel();
        Self {
            engine: StoreEngine {
                history: HistorySync::new(client.clone()),
                initial: InitialSync::new(client, store, initial),
            },
            config,
            commands,
            receiver,
        }
    }

    /// A handle for polling now or changing the activity.
    pub fn handle(&self) -> SchedulerHandle {
        SchedulerHandle {
            commands: self.commands.clone(),
        }
    }

    /// The store being synced.
    pub fn store(&self) -> &Store {
        self.engine.initial.store()
    }

    /// Gives the store back.
    pub fn into_store(self) -> Store {
        self.engine.initial.into_store()
    }

    /// Syncs until `cancel` fires (then returns `Ok`) or a non-transient
    /// error stops it. `on_event` hears about progress, polls, failures and
    /// connectivity.
    pub async fn run(
        &mut self,
        cancel: &CancellationToken,
        on_event: impl FnMut(SyncEvent) + Send,
    ) -> Result<()> {
        drive(
            &mut self.engine,
            &self.config,
            &mut self.receiver,
            cancel,
            on_event,
        )
        .await
    }
}

/// The sync steps the scheduler drives; a trait so the scheduling can be
/// tested without Gmail.
trait Engine: Send {
    /// Whether the initial sync has finished.
    fn initial_complete(&mut self) -> Result<bool>;

    /// Runs (or resumes) the initial sync.
    fn run_initial(
        &mut self,
        cancel: &CancellationToken,
        progress: impl FnMut(&Progress) + Send,
    ) -> impl Future<Output = Result<Progress>> + Send;

    /// Applies the mailbox's history since the last poll.
    fn poll(&mut self, cancel: &CancellationToken) -> impl Future<Output = Result<Changes>> + Send;
}

#[derive(Debug)]
struct StoreEngine {
    initial: InitialSync,
    history: HistorySync,
}

impl Engine for StoreEngine {
    fn initial_complete(&mut self) -> Result<bool> {
        Ok(self.initial.store().sync_state()?.backfill_complete)
    }

    async fn run_initial(
        &mut self,
        cancel: &CancellationToken,
        progress: impl FnMut(&Progress) + Send,
    ) -> Result<Progress> {
        self.initial.run(cancel, progress).await
    }

    async fn poll(&mut self, cancel: &CancellationToken) -> Result<Changes> {
        self.history.run(self.initial.store_mut(), cancel).await
    }
}

/// What one attempt asks for next.
enum Next {
    /// Wait for the poll interval.
    Wait,
    /// Go again at once: the history expired and the store was reset.
    Resync,
}

/// The scheduling loop.
async fn drive<E: Engine>(
    engine: &mut E,
    config: &SchedulerConfig,
    commands: &mut mpsc::UnboundedReceiver<Command>,
    cancel: &CancellationToken,
    mut on_event: impl FnMut(SyncEvent) + Send,
) -> Result<()> {
    let mut activity = Activity::default();
    let mut connectivity = None;
    let mut failures = 0u32;
    let mut resynced = false;
    let mut commands_open = true;
    loop {
        let result = attempt(engine, cancel, &mut on_event).await;
        let finished = Instant::now();
        let wait = match result {
            Ok(next) => {
                failures = 0;
                set_connectivity(&mut connectivity, Connectivity::Online, &mut on_event);
                // A second expiry straight after a resync waits like any
                // other poll rather than looping.
                let again = matches!(next, Next::Resync) && !resynced;
                resynced = again;
                if again {
                    continue;
                }
                None
            }
            Err(Error::Cancelled) => return Ok(()),
            Err(err) if err.is_transient() => {
                failures += 1;
                let state = if is_offline(&err) {
                    Connectivity::Offline
                } else {
                    Connectivity::Online
                };
                set_connectivity(&mut connectivity, state, &mut on_event);
                let mut wait = config.backoff(failures);
                if let Some(Some(after)) = err.rate_limit_retry_after() {
                    wait = wait.max(after);
                }
                tracing::debug!(%err, ?wait, failures, "sync failed; backing off");
                on_event(SyncEvent::Failed {
                    error: Arc::new(err),
                    retry_in: wait,
                });
                Some(wait)
            }
            Err(err) => return Err(err),
        };

        // While healthy the deadline follows the activity; while backing off
        // it is fixed.
        let deadline_for = |activity| finished + wait.unwrap_or_else(|| config.interval(activity));
        let mut deadline = deadline_for(activity);
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => return Ok(()),
                command = commands.recv(), if commands_open => match command {
                    Some(Command::PollNow) => break,
                    Some(Command::SetActivity(new)) => {
                        activity = new;
                        deadline = deadline_for(activity);
                    }
                    // Every handle is gone: keep polling on the timer.
                    None => commands_open = false,
                },
                () = tokio::time::sleep_until(deadline) => break,
            }
        }
    }
}

/// One attempt: the initial sync if it has not finished, then a poll.
async fn attempt<E: Engine>(
    engine: &mut E,
    cancel: &CancellationToken,
    on_event: &mut (impl FnMut(SyncEvent) + Send),
) -> Result<Next> {
    if !engine.initial_complete()? {
        engine
            .run_initial(cancel, |progress| {
                on_event(SyncEvent::Initial(progress.clone()));
            })
            .await?;
    }
    let changes = engine.poll(cancel).await?;
    let resync = changes.full_resync;
    on_event(SyncEvent::Polled(changes));
    Ok(if resync { Next::Resync } else { Next::Wait })
}

fn set_connectivity(
    current: &mut Option<Connectivity>,
    new: Connectivity,
    on_event: &mut impl FnMut(SyncEvent),
) {
    if *current != Some(new) {
        *current = Some(new);
        on_event(SyncEvent::Connectivity(new));
    }
}

/// Whether the request never got an HTTP response: no connection, DNS
/// failure, timeout.
fn is_offline(err: &Error) -> bool {
    matches!(err, Error::Gmail(api::Error::Http(e)) if !e.is_status() && !e.is_decode())
}

#[cfg(test)]
mod tests;
