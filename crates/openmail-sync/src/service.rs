//! `MailService`: the sync engine, the ops queue and the store behind one
//! asynchronous facade, running on a tokio runtime of its own.
//!
//! The UI (or the CLI) talks to it through a [`MailHandle`], whose calls are
//! sent as commands over a channel and answered asynchronously, and hears
//! what changed through [`MailEvents`]. Neither depends on the runtime the
//! caller uses: the futures and the event stream can be awaited on any
//! executor, such as GPUI's.

mod bodies;

use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use openmail_core::{Label, MessageId, ThreadId};
use openmail_gmail::GmailClient;
use openmail_store::{
    LabelCounts, MessageBody, Store, ThreadCursor, ThreadDetail, ThreadPage, ThreadView,
};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use self::bodies::{BodyFetcher, BodyRequest};
use crate::{
    Activity, Changes, Connectivity, Error, InitialSyncConfig, Op, OpEvent, OpId, OpsConfig,
    OpsQueue, Progress, QueuedOp, Result, Scheduler, SchedulerConfig, SchedulerHandle, SyncEvent,
};

/// Worker threads of the service's runtime. The work is mostly waiting on
/// Gmail; two threads keep a long store write from stalling everything else.
const WORKER_THREADS: usize = 2;

/// How a [`MailService`] runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailServiceConfig {
    /// The account's database (see
    /// [`DataDir::account_database`](openmail_store::DataDir::account_database)).
    /// The service opens several connections to it.
    pub database: PathBuf,
    /// The initial sync (and any full resync).
    pub initial: InitialSyncConfig,
    /// How often to poll Gmail's history.
    pub scheduler: SchedulerConfig,
    /// The undo window and retries of the ops queue.
    pub ops: OpsConfig,
    /// After each poll, fetch the bodies of this many of the newest Inbox
    /// messages if they have none (50 by default; 0 turns it off). Other
    /// bodies are fetched when asked for ([`MailHandle::body`]).
    pub prefetch_bodies: usize,
}

impl MailServiceConfig {
    /// The default settings for the database at `database`.
    pub fn new(database: impl Into<PathBuf>) -> Self {
        Self {
            database: database.into(),
            initial: InitialSyncConfig::default(),
            scheduler: SchedulerConfig::default(),
            ops: OpsConfig::default(),
            prefetch_bodies: 50,
        }
    }
}

/// What the service reports. Changes to the store are reported as
/// invalidations ([`ThreadsChanged`](Self::ThreadsChanged),
/// [`LabelsChanged`](Self::LabelsChanged)): the UI queries again what it
/// shows.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum MailEvent {
    /// Where syncing with Gmail is.
    SyncStatus(SyncStatus),
    /// Whether Gmail can be reached, judged from the last sync request.
    Connectivity(Connectivity),
    /// Threads, their labels or flags changed in the store: thread lists,
    /// label counts and open threads may be out of date.
    ThreadsChanged,
    /// The label list changed (a label was added, renamed, recolored or
    /// deleted, or its ID became Gmail's).
    LabelsChanged,
    /// A queued op was sent, cancelled out, is being retried, or failed and
    /// was rolled back (a [`ThreadsChanged`](Self::ThreadsChanged) follows).
    Op(OpEvent),
    /// A message's body was fetched and stored, on request or by prefetch.
    BodyFetched(MessageId),
    /// The ops queue stopped sending, keeping the queue, because nothing more
    /// can be sent until the user acts (signing in again) or the store failed.
    OpsStopped(Arc<Error>),
}

/// Where syncing with Gmail is.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum SyncStatus {
    /// The initial sync (or a full resync) made progress.
    Initial(Progress),
    /// A poll of Gmail's history finished; the store is up to date as of now.
    UpToDate(Changes),
    /// A sync attempt failed for a reason that may pass; the next one starts
    /// in `retry_in` (or sooner on [`MailHandle::poll_now`]).
    Retrying {
        error: Arc<Error>,
        retry_in: Duration,
    },
    /// Syncing stopped for good: signing in failed, the daily quota is spent,
    /// or the store failed. Restart the service once that is resolved.
    Stopped(Arc<Error>),
}

/// The stream of [`MailEvent`]s from a [`MailService`]. It ends after the
/// service shuts down.
#[derive(Debug)]
pub struct MailEvents {
    receiver: mpsc::UnboundedReceiver<MailEvent>,
}

impl MailEvents {
    /// The next event, or `None` once the service has shut down and every
    /// event has been received.
    pub async fn recv(&mut self) -> Option<MailEvent> {
        self.receiver.recv().await
    }

    /// The next event if one is waiting, without waiting.
    pub fn try_recv(&mut self) -> Option<MailEvent> {
        self.receiver.try_recv().ok()
    }
}

/// Sends events, ignoring a dropped receiver.
#[derive(Debug, Clone)]
struct Events(mpsc::UnboundedSender<MailEvent>);

impl Events {
    fn send(&self, event: MailEvent) {
        let _ = self.0.send(event);
    }
}

/// A command for the local task: work on its store connection, which
/// answers through a channel of its own.
type Command = Box<dyn FnOnce(&Local) + Send>;

/// One account's mail: syncs it with Gmail in the background, sends the
/// user's changes, and answers queries from the local store.
///
/// The service runs on a tokio runtime of its own, on a thread of its own:
///
/// - the [`Scheduler`] runs the initial sync until it is complete, then polls
///   Gmail's history;
/// - the [`OpsQueue`] worker sends queued changes;
/// - a local task answers [`MailHandle`] calls (queries, enqueuing and undoing
///   ops) on its own store connection, so they never wait for a sync;
/// - a body fetcher fetches message bodies when asked
///   ([`MailHandle::body`]), batching requests that arrive together, and
///   after each poll prefetches the bodies of the newest Inbox messages
///   ([`MailServiceConfig::prefetch_bodies`]).
///
/// Each has its own connection to the database, which is in WAL mode.
/// Dropping the service (or [`shutdown`](Self::shutdown)) cancels the work
/// and waits for the thread to finish; work is checkpointed, so the next
/// start continues it.
#[derive(Debug)]
pub struct MailService {
    handle: MailHandle,
    cancel: CancellationToken,
    thread: Option<thread::JoinHandle<()>>,
}

impl MailService {
    /// Opens the store and starts syncing the account `client` is signed in
    /// to. Returns the service and its event stream.
    ///
    /// Blocks while the database is opened (and migrated); everything after
    /// that runs in the background.
    pub fn start(client: GmailClient, config: MailServiceConfig) -> Result<(Self, MailEvents)> {
        let MailServiceConfig {
            database,
            initial,
            scheduler,
            ops,
            prefetch_bodies,
        } = config;
        // The first connection creates and migrates the database, so open
        // them one after another.
        let open = || Store::open(&database);
        let local = open()?;
        let sync_store = open()?;
        let ops_store = open()?;
        let body_store = open()?;

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(WORKER_THREADS)
            .thread_name("openmail-mail-worker")
            .enable_all()
            .build()
            .map_err(Error::Runtime)?;

        let (event_tx, receiver) = mpsc::unbounded_channel();
        let events = Events(event_tx);
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (bodies, body_rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let queue = OpsQueue::new(client.clone(), ops);
        let fetcher = BodyFetcher {
            client: client.clone(),
            store: body_store,
            prefetch: prefetch_bodies,
            events: events.clone(),
            requests: body_rx,
        };
        let scheduler = Scheduler::new(client, sync_store, initial, scheduler);
        let handle = MailHandle {
            commands,
            bodies: bodies.clone(),
            scheduler: scheduler.handle(),
        };

        let workers = Workers {
            local: Local {
                store: local,
                queue: queue.clone(),
                events: events.clone(),
            },
            commands: command_rx,
            scheduler,
            queue,
            ops_store,
            fetcher,
            prefetch: bodies,
            events,
            cancel: cancel.clone(),
        };
        let thread = thread::Builder::new()
            .name("openmail-mail".to_owned())
            .spawn(move || {
                runtime.block_on(workers.run());
                // Anything still running is cancelled and dropped here.
                runtime.shutdown_timeout(Duration::from_secs(1));
            })
            .map_err(Error::Runtime)?;

        Ok((
            Self {
                handle,
                cancel,
                thread: Some(thread),
            },
            MailEvents { receiver },
        ))
    }

    /// A handle for querying and changing the mail. Cheap to clone; calls
    /// fail with [`Error::ServiceStopped`] once the service has shut down.
    pub fn handle(&self) -> MailHandle {
        self.handle.clone()
    }

    /// Stops syncing and sending, and waits for the service's thread to
    /// finish. Unsent ops stay queued for the next start.
    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        self.cancel.cancel();
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            tracing::error!("the mail service's thread panicked");
        }
    }
}

impl Drop for MailService {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Talks to a running [`MailService`]. Cheap to clone and `Send`, so it can
/// be handed to UI tasks.
///
/// Queries read the local store and never wait for Gmail. Changes are applied
/// to the store at once and sent to Gmail in the background.
#[derive(Debug, Clone)]
pub struct MailHandle {
    commands: mpsc::UnboundedSender<Command>,
    bodies: mpsc::UnboundedSender<BodyRequest>,
    scheduler: SchedulerHandle,
}

impl MailHandle {
    /// Runs `f` on the local task and returns its answer.
    async fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Local) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(Box::new(move |local| {
                let _ = reply.send(f(local));
            }))
            .map_err(|_| Error::ServiceStopped)?;
        answer.await.map_err(|_| Error::ServiceStopped)?
    }

    /// A page of up to `limit` threads in `view`, newest first, starting
    /// after `after` (see [`Store::thread_page`]).
    pub async fn thread_page(
        &self,
        view: ThreadView,
        after: Option<ThreadCursor>,
        limit: usize,
    ) -> Result<ThreadPage> {
        self.call(move |local| Ok(local.store.thread_page(&view, after.as_ref(), limit)?))
            .await
    }

    /// Total and unread thread counts for every label.
    pub async fn label_counts(&self) -> Result<Vec<LabelCounts>> {
        self.call(|local| Ok(local.store.label_counts()?)).await
    }

    /// Every label.
    pub async fn labels(&self) -> Result<Vec<Label>> {
        self.call(|local| Ok(local.store.labels()?)).await
    }

    /// A thread with its messages, oldest first, and the bodies fetched so
    /// far; `None` if it is not stored.
    pub async fn thread(&self, id: ThreadId) -> Result<Option<ThreadDetail>> {
        self.call(move |local| Ok(local.store.thread_detail(&id)?))
            .await
    }

    /// A message's body: the stored one, or else fetched from Gmail now (and
    /// stored, with a [`MailEvent::BodyFetched`]). `None` if the message no
    /// longer exists or its body could not be read.
    ///
    /// Requests that arrive together are fetched in one batch, ahead of any
    /// prefetching.
    pub async fn body(&self, id: MessageId) -> Result<Option<MessageBody>> {
        let (reply, answer) = oneshot::channel();
        self.bodies
            .send(BodyRequest::Fetch { id, reply })
            .map_err(|_| Error::ServiceStopped)?;
        answer.await.map_err(|_| Error::ServiceStopped)?
    }

    /// Applies `op` to the store and queues it for sending (see
    /// [`OpsQueue::enqueue`]). Queries reflect it when this returns, and a
    /// [`MailEvent::ThreadsChanged`] (and, for label ops,
    /// [`MailEvent::LabelsChanged`]) is sent.
    pub async fn apply(&self, op: Op) -> Result<OpId> {
        self.call(move |local| {
            let changed = Changed::by(&op);
            let id = local.queue.enqueue(&local.store, op)?;
            changed.report(&local.events);
            Ok(id)
        })
        .await
    }

    /// Cancels an op within its undo window and reverts its local change
    /// (see [`OpsQueue::undo`]). Returns `false`, changing nothing, if it is
    /// too late.
    pub async fn undo(&self, id: OpId) -> Result<bool> {
        self.call(move |local| {
            let Some(queued) = local
                .queue
                .queued(&local.store)?
                .into_iter()
                .find(|q| q.id == id)
            else {
                return Ok(false);
            };
            let undone = local.queue.undo(&local.store, id)?;
            if undone {
                Changed::by(&queued.op).report(&local.events);
            }
            Ok(undone)
        })
        .await
    }

    /// The temporary ID of the label a queued [`Op::CreateLabel`] is
    /// creating (see [`OpsQueue::local_label`]).
    pub async fn local_label(&self, id: OpId) -> Result<Option<openmail_core::LabelId>> {
        self.call(move |local| local.queue.local_label(&local.store, id))
            .await
    }

    /// Every op in the queue, oldest first.
    pub async fn queued_ops(&self) -> Result<Vec<QueuedOp>> {
        self.call(|local| local.queue.queued(&local.store)).await
    }

    /// Polls Gmail now (for example when the window gains focus), even while
    /// backing off after failures.
    pub fn poll_now(&self) {
        self.scheduler.poll_now();
    }

    /// Switches between polling often (the app is in use) and seldom.
    pub fn set_activity(&self, activity: Activity) {
        self.scheduler.set_activity(activity);
    }
}

/// What the local task works with.
struct Local {
    store: Store,
    queue: OpsQueue,
    events: Events,
}

/// Which parts of the store an op changes.
#[derive(Debug, Clone, Copy)]
struct Changed {
    threads: bool,
    labels: bool,
}

impl Changed {
    fn by(op: &Op) -> Self {
        match op {
            Op::ModifyLabels { .. } | Op::Trash { .. } | Op::Untrash { .. } | Op::Delete { .. } => {
                Self {
                    threads: true,
                    labels: false,
                }
            }
            Op::CreateLabel { .. } | Op::RenameLabel { .. } | Op::RecolorLabel { .. } => Self {
                threads: false,
                labels: true,
            },
            // Removes the label from every message too.
            Op::DeleteLabel { .. } => Self {
                threads: true,
                labels: true,
            },
        }
    }

    fn report(self, events: &Events) {
        if self.labels {
            events.send(MailEvent::LabelsChanged);
        }
        if self.threads {
            events.send(MailEvent::ThreadsChanged);
        }
    }
}

/// Everything the service runs, moved onto its thread.
struct Workers {
    local: Local,
    commands: mpsc::UnboundedReceiver<Command>,
    scheduler: Scheduler,
    queue: OpsQueue,
    ops_store: Store,
    fetcher: BodyFetcher,
    /// Asks the fetcher to prefetch.
    prefetch: mpsc::UnboundedSender<BodyRequest>,
    events: Events,
    cancel: CancellationToken,
}

impl Workers {
    /// Runs every task until the service is cancelled.
    async fn run(self) {
        let Self {
            local,
            commands,
            scheduler,
            queue,
            ops_store,
            fetcher,
            prefetch,
            events,
            cancel,
        } = self;
        let mut tasks = JoinSet::new();
        tasks.spawn(run_local(local, commands, cancel.clone()));
        tasks.spawn(run_sync(
            scheduler,
            prefetch,
            events.clone(),
            cancel.clone(),
        ));
        tasks.spawn(fetcher.run(cancel.clone()));
        tasks.spawn(run_ops(queue, ops_store, events, cancel.clone()));
        cancel.cancelled().await;
        while let Some(result) = tasks.join_next().await {
            if let Err(err) = result {
                tracing::error!(%err, "a mail service task failed");
            }
        }
    }
}

/// Answers [`MailHandle`] calls, one at a time.
async fn run_local(
    local: Local,
    mut commands: mpsc::UnboundedReceiver<Command>,
    cancel: CancellationToken,
) {
    loop {
        let command = tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            command = commands.recv() => command,
        };
        match command {
            Some(command) => command(&local),
            // Every handle is gone; the service itself keeps one, so this is
            // shutdown.
            None => return,
        }
    }
}

/// Runs the scheduler, reporting what it does, and prefetches bodies after
/// each poll.
async fn run_sync(
    mut scheduler: Scheduler,
    prefetch: mpsc::UnboundedSender<BodyRequest>,
    events: Events,
    cancel: CancellationToken,
) {
    let mut last: Option<Progress> = None;
    let result = scheduler
        .run(&cancel, |event| {
            let polled = matches!(&event, SyncEvent::Polled(changes) if !changes.full_resync);
            report_sync(&events, &mut last, event);
            if polled {
                let _ = prefetch.send(BodyRequest::Prefetch);
            }
        })
        .await;
    if let Err(err) = result {
        tracing::warn!(%err, "syncing stopped");
        events.send(MailEvent::SyncStatus(SyncStatus::Stopped(Arc::new(err))));
    }
}

/// Turns a scheduler event into service events. `last` is the previous
/// initial-sync progress, to tell what a new report changed.
fn report_sync(events: &Events, last: &mut Option<Progress>, event: SyncEvent) {
    match event {
        SyncEvent::Initial(progress) => {
            let before = last.as_ref();
            let labels = before.is_none_or(|b| b.labels != progress.labels);
            let threads = before.is_none_or(|b| {
                (b.threads, b.deleted_threads) != (progress.threads, progress.deleted_threads)
            });
            if labels && progress.labels > 0 {
                events.send(MailEvent::LabelsChanged);
            }
            if threads && (progress.threads > 0 || progress.deleted_threads > 0) {
                events.send(MailEvent::ThreadsChanged);
            }
            *last = Some(progress.clone());
            events.send(MailEvent::SyncStatus(SyncStatus::Initial(progress)));
        }
        SyncEvent::Polled(changes) => {
            *last = None;
            if changes.labels_synced {
                events.send(MailEvent::LabelsChanged);
            }
            if !changes.is_empty() {
                events.send(MailEvent::ThreadsChanged);
            }
            // After a full resync the initial sync starts at once and reports
            // its own progress.
            if !changes.full_resync {
                events.send(MailEvent::SyncStatus(SyncStatus::UpToDate(changes)));
            }
        }
        SyncEvent::Connectivity(state) => events.send(MailEvent::Connectivity(state)),
        SyncEvent::Failed { error, retry_in } => {
            events.send(MailEvent::SyncStatus(SyncStatus::Retrying {
                error,
                retry_in,
            }));
        }
    }
}

/// Runs the ops queue's replay worker, reporting what it does.
async fn run_ops(queue: OpsQueue, mut store: Store, events: Events, cancel: CancellationToken) {
    let result = queue
        .run(&mut store, &cancel, |event| {
            let changed = match &event {
                // Rolled back.
                OpEvent::Failed { op, .. } => Some(Changed::by(op)),
                // The label now has Gmail's ID, in the label list and on
                // messages.
                OpEvent::Sent {
                    op: Op::CreateLabel { .. },
                    ..
                } => Some(Changed {
                    threads: true,
                    labels: true,
                }),
                _ => None,
            };
            events.send(MailEvent::Op(event));
            if let Some(changed) = changed {
                changed.report(&events);
            }
        })
        .await;
    if let Err(err) = result {
        tracing::warn!(%err, "the ops queue stopped");
        events.send(MailEvent::OpsStopped(Arc::new(err)));
    }
}

#[cfg(test)]
mod tests;
