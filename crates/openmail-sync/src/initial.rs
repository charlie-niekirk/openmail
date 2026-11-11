//! The initial sync: labels, then Inbox threads, then a backfill of older mail.

use std::collections::{HashMap, HashSet};
use std::future::Future;

use openmail_core::{HistoryId, Label, LabelId, MessageId, SystemLabel, ThreadId};
use openmail_gmail::GmailClient;
use openmail_gmail::api::{
    self, ApiThread, LearnedBudget, ListRequest, MAX_BATCH_SIZE, MessageFormat, ThreadFormat,
};
use openmail_mime::ParsedMessage;
use openmail_store::{InlinePart, MessageBody, QuotaBudget, Store, SyncState};
use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};
use tokio_util::sync::CancellationToken;

use crate::{Error, Result, ops};

/// The headers kept with thread metadata during the initial sync.
pub const SYNC_HEADERS: &[&str] = &["From", "To", "Cc", "Subject", "Date", "Message-ID"];

/// Messages per `messages.list` page when indexing which threads have a
/// single message (Gmail's maximum).
const INDEX_PAGE_SIZE: u32 = 500;

/// How far back the backfill goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cutoff {
    /// The whole mailbox.
    All,
    /// Threads with mail in the last this many days.
    Days(u32),
    /// Threads with mail after this moment.
    Since(OffsetDateTime),
}

impl Default for Cutoff {
    /// The last 90 days.
    fn default() -> Self {
        Self::Days(90)
    }
}

impl Cutoff {
    /// The cutoff as Unix seconds, for Gmail's `after:` search operator.
    fn after(self, now: OffsetDateTime) -> Option<i64> {
        match self {
            Self::All => None,
            Self::Days(days) => Some((now - Duration::days(i64::from(days))).unix_timestamp()),
            Self::Since(date) => Some(date.unix_timestamp()),
        }
    }
}

/// How the initial sync runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitialSyncConfig {
    /// How far back the sync goes, in the Inbox and the backfill alike. Older
    /// threads are left for on-demand fetching.
    ///
    /// Fixed when a sync starts: a resumed sync keeps the cutoff it started with.
    pub cutoff: Cutoff,
    /// Threads per `threads.list` page. Each page's metadata is fetched with
    /// the client's batch size and concurrency (see
    /// [`GmailClientBuilder::batch_size`](openmail_gmail::api::GmailClientBuilder::batch_size))
    /// and written in one transaction.
    pub page_size: u32,
    /// Fetch the bodies of this many of the newest Inbox messages after the
    /// Inbox is synced. `None` (the default) fetches no bodies.
    pub prefetch_inbox_bodies: Option<usize>,
    /// How long to pause when Gmail's per-user rate limit outlasts the
    /// client's own retries, before resuming from the checkpoint (longer if
    /// Gmail asks with `Retry-After`).
    pub rate_limit_pause: std::time::Duration,
    /// How many such pauses in a row, with no progress between them, before
    /// the sync gives up with the rate-limit error.
    pub max_rate_limit_pauses: u32,
    /// Fetch threads that have a single message with `messages.get` (5 quota
    /// units) instead of `threads.get` (10). On by default; see
    /// [`InitialSync`].
    pub single_message_fetch: bool,
}

impl Default for InitialSyncConfig {
    fn default() -> Self {
        Self {
            cutoff: Cutoff::default(),
            page_size: 100,
            prefetch_inbox_bodies: None,
            rate_limit_pause: std::time::Duration::from_secs(60),
            max_rate_limit_pauses: 5,
            single_message_fetch: true,
        }
    }
}

/// Where the initial sync is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Phase {
    /// Syncing the label list.
    #[default]
    Labels,
    /// Syncing Inbox threads, newest first, down to the cutoff.
    Inbox,
    /// Fetching bodies of the newest Inbox messages.
    Prefetch,
    /// Syncing the rest of the mailbox, newest first, down to the cutoff.
    Backfill,
    /// Finished.
    Done,
}

/// Counts reported as the sync goes, for this run only (a resumed sync starts
/// from zero).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Progress {
    pub phase: Phase,
    /// Whether this run continued an interrupted sync.
    pub resumed: bool,
    /// Labels stored.
    pub labels: usize,
    /// Threads fetched and stored.
    pub threads: u64,
    /// Messages stored with those threads.
    pub messages: u64,
    /// Listed threads not fetched because they were already stored unchanged
    /// (or were deleted before they could be fetched).
    pub skipped_threads: u64,
    /// Message bodies prefetched.
    pub bodies: u64,
    /// Stored threads deleted because they no longer exist on the server:
    /// gone when fetched, or (at the end of a sync that started afresh) not
    /// listed although within the cutoff.
    pub deleted_threads: u64,
    /// Set while the sync is paused for Gmail's rate limit: how long it waits
    /// before resuming.
    pub rate_limit_wait: Option<std::time::Duration>,
    /// Requests and batch items the client sent again after a retryable
    /// failure (see [`ClientStats`](openmail_gmail::api::ClientStats)).
    pub retried_requests: u64,
    /// Requests and batch items Gmail rejected with a rate limit.
    pub rate_limited_requests: u64,
    /// The quota units per second the client paces itself to right now;
    /// `None` if unlimited.
    pub units_per_second: Option<u32>,
    /// The most quota units the client allows itself per trailing minute
    /// (learned from Gmail's rate limiting unless the rate is fixed); `None`
    /// if unlimited.
    pub budget_per_minute: Option<u32>,
    /// The quota units the client charged in the trailing minute, failed
    /// attempts included; `None` if unlimited.
    pub spent_last_minute: Option<u32>,
}

impl Progress {
    /// Counts that grow whenever the sync gets further.
    fn advanced(&self) -> (u64, u64, u64) {
        (self.threads, self.skipped_threads, self.bodies)
    }
}

/// Downloads a mailbox's labels and thread metadata into a [`Store`].
///
/// The order is: record the mailbox's current `historyId` (so incremental sync
/// can later catch up on anything that changes meanwhile), sync labels, sync
/// Inbox threads, optionally prefetch the newest Inbox bodies, then backfill
/// the rest of the mailbox. Both listings go newest-first down to the
/// [`Cutoff`].
///
/// Each page of threads is written in one transaction together with a
/// checkpoint in `sync_state`, so a sync that fails or is cancelled continues
/// from the next page when run again.
///
/// Threads are listed with `threads.list`, and those already stored with the
/// same history ID are skipped. Most threads have a single message, and those
/// are fetched with `messages.get`, at half the quota cost of `threads.get`.
/// To tell them apart without fetching, the sync pages through `messages.list`
/// with the same cutoff alongside the threads: a thread has a single message
/// if the newest message listed for it is the thread's first message, whose
/// ID Gmail uses as the thread ID. A thread whose first message is older than
/// the cutoff is never listed with its own ID, so it is always fetched whole.
///
/// The client's learned quota budget is saved in the store as the sync goes,
/// and a client that has not learned one yet starts at the saved one.
#[derive(Debug)]
pub struct InitialSync {
    client: GmailClient,
    store: Store,
    config: InitialSyncConfig,
    /// The quota budget last saved in the store.
    saved_budget: Option<QuotaBudget>,
    /// The `messages.list` index of this run.
    index: Option<MessageIndex>,
}

impl InitialSync {
    /// A sync of the account `client` is signed in to, into `store`.
    pub fn new(client: GmailClient, store: Store, config: InitialSyncConfig) -> Self {
        Self {
            client,
            store,
            config,
            saved_budget: None,
            index: None,
        }
    }

    /// The store being synced into.
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// The store being synced into, mutably (for [`HistorySync::run`](crate::HistorySync::run)).
    pub fn store_mut(&mut self) -> &mut Store {
        &mut self.store
    }

    /// Gives the store back.
    pub fn into_store(self) -> Store {
        self.store
    }

    /// Makes the next [`run`](Self::run) a full resync, as when Gmail no
    /// longer has history back to the stored `historyId`.
    ///
    /// Clears the history ID, the checkpoint and `backfill_complete`, so the
    /// next run starts afresh: it records the mailbox's current history ID,
    /// lists every thread down to the configured cutoff, and fetches those
    /// whose history ID differs from the stored copy. Stored rows are kept
    /// meanwhile. When it finishes, stored threads within the cutoff that it
    /// did not list are deleted, since they no longer exist (or are now in
    /// Spam or Trash, which the sync does not list); older threads are kept.
    pub fn reset(store: &Store) -> Result<()> {
        store.transaction(|store| {
            let state = store.sync_state()?;
            store.set_sync_state(&SyncState {
                history_id: None,
                backfill_cursor: None,
                backfill_complete: false,
                last_full_sync: state.last_full_sync,
            })
        })?;
        Ok(())
    }

    /// Runs (or resumes) the sync until it finishes, fails or `cancel` fires.
    ///
    /// `progress` is called as each phase starts and after each page or batch.
    /// Returns the final progress; if the sync had already finished, that is
    /// [`Phase::Done`] with nothing done.
    ///
    /// If Gmail's per-user rate limit outlasts the client's retries, the sync
    /// reports [`Progress::rate_limit_wait`], pauses for
    /// [`InitialSyncConfig::rate_limit_pause`] and resumes from its checkpoint,
    /// up to [`InitialSyncConfig::max_rate_limit_pauses`] times in a row
    /// without progress.
    pub async fn run(
        &mut self,
        cancel: &CancellationToken,
        mut progress: impl FnMut(&Progress),
    ) -> Result<Progress> {
        // Retry counts come from the client, relative to the start of the run.
        let client = self.client.clone();
        let start = client.stats();
        let with_stats = move |report: &Progress| {
            let now = client.stats();
            Progress {
                retried_requests: now.retries - start.retries,
                rate_limited_requests: now.rate_limited - start.rate_limited,
                units_per_second: now.units_per_second,
                budget_per_minute: now.budget_per_minute,
                spent_last_minute: now.spent_last_minute,
                ..report.clone()
            }
        };
        let mut progress = |report: &Progress| progress(&with_stats(report));
        self.seed_budget()?;
        // Messages may have arrived since an earlier run listed them.
        self.index = None;
        let mut report = Progress::default();
        let mut pauses = 0;
        let mut at_last_pause = report.advanced();
        let mut first = true;
        loop {
            let result = self
                .run_once(cancel, first, &mut report, &mut progress)
                .await;
            let saved = self.save_budget();
            let err = match result {
                Ok(()) => {
                    saved?;
                    return Ok(with_stats(&report));
                }
                Err(err) => {
                    if let Err(save_err) = saved {
                        tracing::warn!(%save_err, "could not save the quota budget");
                    }
                    err
                }
            };
            first = false;
            if report.advanced() != at_last_pause {
                pauses = 0;
            }
            let Some(retry_after) = err.rate_limit_retry_after() else {
                return Err(err);
            };
            if pauses >= self.config.max_rate_limit_pauses {
                return Err(err);
            }
            pauses += 1;
            at_last_pause = report.advanced();

            let wait = retry_after.map_or(self.config.rate_limit_pause, |after| {
                after.max(self.config.rate_limit_pause)
            });
            tracing::info!(?wait, "Gmail rate limit; pausing the sync");
            report.rate_limit_wait = Some(wait);
            progress(&report);
            tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(Error::Cancelled),
                () = tokio::time::sleep(wait) => {}
            }
            report.rate_limit_wait = None;
        }
    }

    // The async helpers take `&mut self` so their futures are `Send`: `Store`
    // is `Send` but not `Sync`.

    /// One attempt at the sync, from the stored checkpoint. `first` is false
    /// when continuing after a rate-limit pause in the same run.
    async fn run_once(
        &mut self,
        cancel: &CancellationToken,
        first: bool,
        report: &mut Progress,
        progress: &mut impl FnMut(&Progress),
    ) -> Result<()> {
        let state = self.store.sync_state()?;
        if state.backfill_complete {
            report.phase = Phase::Done;
            return Ok(());
        }

        let mut checkpoint = match state.backfill_cursor.as_deref().map(Checkpoint::decode) {
            Some(Some(checkpoint)) => {
                report.resumed |= first;
                checkpoint
            }
            unreadable => {
                if unreadable.is_some() {
                    tracing::warn!("unreadable sync checkpoint; starting the sync again");
                }
                self.start(cancel).await?
            }
        };

        progress(report);
        self.sync_labels(cancel, report).await?;
        progress(report);

        loop {
            match checkpoint.phase {
                CheckpointPhase::Inbox => {
                    report.phase = Phase::Inbox;
                    progress(report);
                    let request = checkpoint.listing().label(SystemLabel::Inbox.id());
                    checkpoint = self
                        .sync_listing(cancel, checkpoint, request, report, progress)
                        .await?;
                }
                CheckpointPhase::Prefetch => {
                    if let Some(limit) = self.config.prefetch_inbox_bodies {
                        report.phase = Phase::Prefetch;
                        progress(report);
                        self.prefetch_bodies(cancel, limit, report, progress)
                            .await?;
                    }
                    checkpoint = checkpoint.next_phase();
                    self.save(&checkpoint)?;
                }
                CheckpointPhase::Backfill => {
                    report.phase = Phase::Backfill;
                    progress(report);
                    let request = checkpoint.listing();
                    checkpoint = self
                        .sync_listing(cancel, checkpoint, request, report, progress)
                        .await?;
                }
                CheckpointPhase::Done => break,
            }
        }

        report.phase = Phase::Done;
        progress(report);
        Ok(())
    }

    /// Starts a new sync: records the current history ID and the first
    /// checkpoint together.
    async fn start(&mut self, cancel: &CancellationToken) -> Result<Checkpoint> {
        let profile = cancellable(cancel, self.client.get_profile()).await?;
        let checkpoint = Checkpoint {
            phase: CheckpointPhase::Inbox,
            page_token: None,
            after: self.config.cutoff.after(OffsetDateTime::now_utc()),
            inbox_cutoff: true,
            sweep: true,
        };
        self.store.transaction(|store| {
            store.clear_seen_threads()?;
            let state = SyncState {
                history_id: Some(profile.history_id),
                backfill_cursor: Some(checkpoint.encode()),
                backfill_complete: false,
                last_full_sync: None,
            };
            store.set_sync_state(&state)
        })?;
        Ok(checkpoint)
    }

    async fn sync_labels(
        &mut self,
        cancel: &CancellationToken,
        report: &mut Progress,
    ) -> Result<()> {
        let requested_at = OffsetDateTime::now_utc();
        let labels: Vec<Label> = cancellable(cancel, self.client.list_labels())
            .await?
            .into_iter()
            .map(Label::from)
            .collect();
        self.store.transaction(|store| {
            store.replace_labels(&labels)?;
            ops::rebase(store, &HashSet::new(), true, requested_at)
        })?;
        report.labels = labels.len();
        Ok(())
    }

    /// Syncs every page of `request` from the checkpoint's page token, and
    /// returns the checkpoint for the next phase.
    async fn sync_listing(
        &mut self,
        cancel: &CancellationToken,
        mut checkpoint: Checkpoint,
        request: ListRequest,
        report: &mut Progress,
        progress: &mut impl FnMut(&Progress),
    ) -> Result<Checkpoint> {
        let phase = checkpoint.phase;
        while checkpoint.phase == phase {
            let request = ListRequest {
                page_token: checkpoint.page_token.clone(),
                max_results: Some(self.config.page_size),
                ..request.clone()
            };
            let requested_at = OffsetDateTime::now_utc();
            let page = cancellable(cancel, self.client.list_threads(&request)).await?;
            let listed: Vec<ThreadId> = page.threads.iter().map(|t| t.id.clone()).collect();
            let fetch = self.changed_threads(page.threads)?;
            let (threads, gone) = self.fetch_page(cancel, checkpoint.after, fetch).await?;

            checkpoint = match page.next_page_token.filter(|t| !t.is_empty()) {
                Some(token) => checkpoint.with_page_token(token),
                None => checkpoint.next_phase(),
            };
            let deleted = self.store.transaction(|store| {
                store.replace_threads(&threads)?;
                let mut deleted = 0;
                for id in &gone {
                    deleted += u64::from(store.delete_thread(id)?);
                }
                if checkpoint.sweep {
                    store.mark_threads_seen(&listed)?;
                }
                let written = threads.iter().map(|(t, _)| &t.id).chain(&gone).collect();
                ops::rebase(store, &written, false, requested_at)?;
                Ok::<_, Error>(deleted + save_checkpoint(store, &checkpoint)?)
            })?;

            report.threads += threads.len() as u64;
            report.messages += threads.iter().map(|(_, m)| m.len() as u64).sum::<u64>();
            report.skipped_threads += (listed.len() - threads.len()) as u64;
            report.deleted_threads += deleted;
            self.save_budget()?;
            progress(report);
        }
        Ok(checkpoint)
    }

    /// The listed threads whose stored copy is missing or has another history
    /// ID, with their listed history IDs.
    fn changed_threads(&self, listed: Vec<ApiThread>) -> Result<Vec<ListedThread>> {
        let ids: Vec<ThreadId> = listed.iter().map(|t| t.id.clone()).collect();
        let stored = self.store.thread_history_ids(&ids)?;
        Ok(listed
            .into_iter()
            .filter(|t| t.history_id.is_none() || t.history_id != stored.get(&t.id).copied())
            .map(|t| (t.id, t.history_id))
            .collect())
    }

    /// Fetches the metadata of a page's changed threads: those with a single
    /// message with `messages.get`, the rest with `threads.get`, batched and
    /// at the same time. Threads deleted since they were listed are returned
    /// separately.
    async fn fetch_page(
        &mut self,
        cancel: &CancellationToken,
        after: Option<i64>,
        fetch: Vec<ListedThread>,
    ) -> Result<FetchedThreads> {
        if fetch.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        let (single, whole) = if self.config.single_message_fetch {
            let index = match &mut self.index {
                Some(index) if index.after == after => index,
                index => index.insert(MessageIndex::new(after)),
            };
            index.split(&self.client, cancel, fetch).await?
        } else {
            (Vec::new(), fetch.into_iter().map(|(id, _)| id).collect())
        };
        tracing::debug!(
            single = single.len(),
            whole = whole.len(),
            "fetching thread metadata"
        );
        let (mut singles, mut wholes) = tokio::try_join!(
            fetch_single_messages(&self.client, cancel, &single),
            fetch_threads(&self.client, cancel, &whole),
        )?;
        singles.0.append(&mut wholes.0);
        singles.1.append(&mut wholes.1);
        Ok(singles)
    }

    /// Fetches and stores bodies for the newest `limit` Inbox messages that do
    /// not have one yet.
    async fn prefetch_bodies(
        &mut self,
        cancel: &CancellationToken,
        limit: usize,
        report: &mut Progress,
        progress: &mut impl FnMut(&Progress),
    ) -> Result<()> {
        let ids = self
            .store
            .recent_messages_without_body(&LabelId::from(SystemLabel::Inbox), limit)?;
        for chunk in ids.chunks(MAX_BATCH_SIZE) {
            let results = cancellable(
                cancel,
                self.client.get_messages(chunk, MessageFormat::Raw, &[]),
            )
            .await?;
            let fetched_at = OffsetDateTime::now_utc();
            let mut bodies = Vec::with_capacity(chunk.len());
            for (id, result) in chunk.iter().zip(results) {
                match result {
                    Ok(message) => {
                        if let Some(body) = parse_body(id, message.raw.as_deref(), fetched_at) {
                            bodies.push((id, body));
                        }
                    }
                    Err(api::Error::NotFound { .. }) => {
                        tracing::debug!(message = %id, "message deleted before its body was fetched");
                    }
                    Err(err) => return Err(err.into()),
                }
            }
            self.store.transaction(|store| {
                bodies
                    .iter()
                    .try_for_each(|(id, body)| store.upsert_body(id, body))
            })?;
            report.bodies += bodies.len() as u64;
            progress(report);
        }
        Ok(())
    }

    /// Starts the client at the quota budget saved by an earlier sync, unless
    /// it has learned its own.
    fn seed_budget(&mut self) -> Result<()> {
        let Some(saved) = self.store.quota_budget()? else {
            return Ok(());
        };
        self.saved_budget = Some(saved);
        let seeded = self.client.seed_budget(LearnedBudget {
            units_per_minute: saved.units_per_minute,
            budget: saved.budget,
        });
        if seeded {
            tracing::debug!(budget = saved.budget, "starting at the saved quota budget");
        }
        Ok(())
    }

    /// Saves the client's quota budget if it changed.
    fn save_budget(&mut self) -> Result<()> {
        let Some(learned) = self.client.learned_budget() else {
            return Ok(());
        };
        let budget = QuotaBudget {
            budget: learned.budget,
            units_per_minute: learned.units_per_minute,
        };
        if self.saved_budget != Some(budget) {
            self.store.set_quota_budget(Some(budget))?;
            self.saved_budget = Some(budget);
        }
        Ok(())
    }

    fn save(&self, checkpoint: &Checkpoint) -> Result<()> {
        self.store
            .transaction(|store| save_checkpoint(store, checkpoint))?;
        Ok(())
    }
}

/// Fetched threads with their messages, and the IDs of threads that turned
/// out to be deleted.
pub(crate) type FetchedThreads = (
    Vec<(openmail_core::Thread, Vec<openmail_core::Message>)>,
    Vec<ThreadId>,
);

/// A thread from `threads.list`, with its history ID there.
type ListedThread = (ThreadId, Option<HistoryId>);

/// Which threads have a single message, from paging through `messages.list`
/// (newest first, with the sync's cutoff) as far as the threads asked about.
#[derive(Debug)]
struct MessageIndex {
    /// The cutoff the listing uses.
    after: Option<i64>,
    /// The next page, unless the listing is finished.
    next_page: Option<String>,
    finished: bool,
    /// For every thread listed so far: whether the newest of its messages
    /// listed is its first message (whose ID is the thread ID), so the
    /// thread has only that message.
    single: HashMap<ThreadId, bool>,
}

impl MessageIndex {
    fn new(after: Option<i64>) -> Self {
        Self {
            after,
            next_page: None,
            finished: false,
            single: HashMap::new(),
        }
    }

    /// Splits `threads` into those with a single message (to fetch with
    /// `messages.get`) and the rest (to fetch whole), listing more messages
    /// until every thread has been seen or the listing ends. A thread never
    /// seen (its messages are all in Spam or Trash, say) is fetched whole.
    async fn split(
        &mut self,
        client: &GmailClient,
        cancel: &CancellationToken,
        threads: Vec<ListedThread>,
    ) -> Result<(Vec<ListedThread>, Vec<ThreadId>)> {
        while !self.finished && threads.iter().any(|(id, _)| !self.single.contains_key(id)) {
            let mut request = match self.after {
                Some(after) => ListRequest::new().query(format!("after:{after}")),
                None => ListRequest::new(),
            };
            request.page_token = self.next_page.take();
            request.max_results = Some(INDEX_PAGE_SIZE);
            let page = cancellable(cancel, client.list_messages(&request)).await?;
            for message in page.messages {
                let first = message.id.as_str() == message.thread_id.as_str();
                // Newest first: only a thread's first listing counts.
                self.single.entry(message.thread_id).or_insert(first);
            }
            self.next_page = page.next_page_token.filter(|t| !t.is_empty());
            self.finished = self.next_page.is_none();
        }
        let (single, whole): (Vec<_>, Vec<_>) = threads
            .into_iter()
            .partition(|(id, _)| self.single.get(id).copied().unwrap_or(false));
        Ok((single, whole.into_iter().map(|(id, _)| id).collect()))
    }
}

/// Fetches threads that have a single message, whose ID is the thread ID,
/// with `messages.get` (metadata, with [`SYNC_HEADERS`]). Each thread keeps
/// the history ID it was listed with, so the unchanged-thread check compares
/// like with like. Threads whose message no longer exists are returned
/// separately.
async fn fetch_single_messages(
    client: &GmailClient,
    cancel: &CancellationToken,
    threads: &[ListedThread],
) -> Result<FetchedThreads> {
    if threads.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let ids: Vec<MessageId> = threads
        .iter()
        .map(|(id, _)| MessageId::new(id.as_str()))
        .collect();
    let results = cancellable(
        cancel,
        client.get_messages(&ids, MessageFormat::Metadata, SYNC_HEADERS),
    )
    .await?;
    let mut fetched = Vec::with_capacity(threads.len());
    let mut gone = Vec::new();
    for ((id, history_id), result) in threads.iter().zip(results) {
        match result {
            Ok(message) => {
                let thread = ApiThread {
                    id: id.clone(),
                    snippet: None,
                    history_id: *history_id,
                    messages: vec![message],
                };
                fetched.push(thread.into_domain()?);
            }
            Err(api::Error::NotFound { .. }) => {
                tracing::debug!(thread = %id, "thread deleted before it was fetched");
                gone.push(id.clone());
            }
            Err(err) => return Err(err.into()),
        }
    }
    Ok((fetched, gone))
}

/// Fetches thread metadata (with [`SYNC_HEADERS`]) in batches. Threads that
/// no longer exist are returned separately.
pub(crate) async fn fetch_threads(
    client: &GmailClient,
    cancel: &CancellationToken,
    ids: &[ThreadId],
) -> Result<FetchedThreads> {
    if ids.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let results = cancellable(
        cancel,
        client.get_threads(ids, ThreadFormat::Metadata, SYNC_HEADERS),
    )
    .await?;
    let mut threads = Vec::with_capacity(ids.len());
    let mut gone = Vec::new();
    for (id, result) in ids.iter().zip(results) {
        match result {
            Ok(thread) => threads.push(thread.into_domain()?),
            Err(api::Error::NotFound { .. }) => {
                tracing::debug!(thread = %id, "thread deleted before it was fetched");
                gone.push(id.clone());
            }
            Err(err) => return Err(err.into()),
        }
    }
    Ok((threads, gone))
}

/// Parses a `format=raw` message into a body. A body that does not parse is
/// skipped with a warning: it will be fetched again when the message is opened.
pub(crate) fn parse_body(
    id: &MessageId,
    raw: Option<&str>,
    fetched_at: OffsetDateTime,
) -> Option<MessageBody> {
    let Some(raw) = raw else {
        tracing::warn!(message = %id, "Gmail returned no raw message");
        return None;
    };
    match ParsedMessage::parse_base64url(raw) {
        Ok(parsed) => Some(MessageBody {
            text: parsed.plain_text(),
            inline: inline_images(&parsed),
            html: parsed.html,
            fetched_at,
        }),
        Err(err) => {
            tracing::warn!(message = %id, %err, "could not parse a message body");
            None
        }
    }
}

/// The most inline image data kept for one body. Past it, the remaining
/// `cid:` images are left out (they show as missing images).
const MAX_INLINE_BYTES: usize = 10 * 1024 * 1024;

/// The images `parsed`'s HTML references with `cid:` URLs, to keep with its
/// body: parts with a `Content-ID` the HTML mentions whose type (declared or,
/// for a generic type, sniffed) is an image, up to [`MAX_INLINE_BYTES`].
fn inline_images(parsed: &ParsedMessage) -> Vec<InlinePart> {
    let Some(html) = parsed.html.as_deref().filter(|html| html.contains("cid:")) else {
        return Vec::new();
    };
    let candidates = parsed.inline.values().chain(&parsed.attachments);
    let mut seen = HashSet::new();
    let mut total = 0;
    let mut images = Vec::new();
    for part in candidates {
        let Some(content_id) = part.content_id.as_deref() else {
            continue;
        };
        if !html.contains(content_id) || !seen.insert(content_id) {
            continue;
        }
        let Some(mime_type) = image_type(&part.mime_type, part.bytes()) else {
            continue;
        };
        total += part.size();
        if total > MAX_INLINE_BYTES {
            tracing::debug!(content_id, "inline images past the size limit are left out");
            break;
        }
        images.push(InlinePart {
            content_id: content_id.to_owned(),
            mime_type,
            data: part.bytes().to_vec(),
        });
    }
    images
}

/// The image type of a part declared as `mime_type`: the declared type if it
/// is an image, or else the type its first bytes show.
fn image_type(mime_type: &str, data: &[u8]) -> Option<String> {
    if mime_type.starts_with("image/") {
        return Some(mime_type.to_owned());
    }
    let sniffed = if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if data.starts_with(&[0xff, 0xd8, 0xff]) {
        "image/jpeg"
    } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        "image/gif"
    } else if data.len() >= 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP" {
        "image/webp"
    } else {
        return None;
    };
    Some(sniffed.to_owned())
}

/// Saves `checkpoint` as the place to resume from or, once it is
/// [done](CheckpointPhase::Done), marks the sync complete and deletes stored
/// threads within the cutoff that the sync did not list (if it tracked them).
/// Returns how many threads were deleted.
fn save_checkpoint(store: &Store, checkpoint: &Checkpoint) -> Result<u64, openmail_store::Error> {
    let mut state = store.sync_state()?;
    let mut deleted = 0;
    if checkpoint.phase == CheckpointPhase::Done {
        state.backfill_cursor = None;
        state.backfill_complete = true;
        state.last_full_sync = Some(OffsetDateTime::now_utc());
        if checkpoint.sweep {
            let since = checkpoint
                .after
                .map(|secs| {
                    OffsetDateTime::from_unix_timestamp(secs).map_err(|err| {
                        openmail_store::Error::Corrupt(format!("checkpoint cutoff: {err}"))
                    })
                })
                .transpose()?;
            deleted = store.delete_unseen_threads(since)? as u64;
            if deleted > 0 {
                tracing::info!(deleted, "deleted threads that no longer exist");
            }
        }
    } else {
        state.backfill_cursor = Some(checkpoint.encode());
    }
    store.set_sync_state(&state)?;
    Ok(deleted)
}

/// Waits for `future` unless `cancel` fires first.
pub(crate) async fn cancellable<T>(
    cancel: &CancellationToken,
    future: impl Future<Output = api::Result<T>>,
) -> Result<T> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(Error::Cancelled),
        result = future => result.map_err(Error::from),
    }
}

/// Where an interrupted sync resumes, stored as JSON in
/// `sync_state.backfill_cursor`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Checkpoint {
    phase: CheckpointPhase,
    /// The next `threads.list` page of the phase; `None` for its first page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    page_token: Option<String>,
    /// The cutoff in Unix seconds, fixed when the sync starts (page tokens
    /// belong to one query). `None` syncs everything.
    after: Option<i64>,
    /// Whether the Inbox listing uses the cutoff too. Checkpoints written
    /// before it did lack the field, and their Inbox page token belongs to an
    /// unbounded listing.
    #[serde(default)]
    inbox_cutoff: bool,
    /// Whether listed threads are recorded, so that stored threads the sync
    /// did not list can be deleted when it finishes. Checkpoints written
    /// before this lack it, and have not recorded their earlier pages.
    #[serde(default)]
    sweep: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CheckpointPhase {
    Inbox,
    Prefetch,
    Backfill,
    /// Never stored: a finished sync has no checkpoint.
    Done,
}

impl Checkpoint {
    fn encode(&self) -> String {
        serde_json::to_string(self).expect("a checkpoint always serializes")
    }

    /// Reads a stored checkpoint. One from before the Inbox used the cutoff
    /// restarts the Inbox listing with it (stored threads are not fetched
    /// again, since their history IDs match).
    fn decode(json: &str) -> Option<Self> {
        let mut checkpoint: Self = serde_json::from_str(json).ok()?;
        if checkpoint.phase == CheckpointPhase::Done {
            return None;
        }
        if !checkpoint.inbox_cutoff {
            if checkpoint.phase == CheckpointPhase::Inbox && checkpoint.after.is_some() {
                tracing::info!("restarting the Inbox listing with the sync's cutoff");
                checkpoint.page_token = None;
            }
            checkpoint.inbox_cutoff = true;
        }
        Some(checkpoint)
    }

    /// A `threads.list` request for this checkpoint's cutoff.
    fn listing(&self) -> ListRequest {
        match self.after {
            Some(after) => ListRequest::new().query(format!("after:{after}")),
            None => ListRequest::new(),
        }
    }

    fn with_page_token(self, token: String) -> Self {
        Self {
            page_token: Some(token),
            ..self
        }
    }

    fn next_phase(self) -> Self {
        let phase = match self.phase {
            CheckpointPhase::Inbox => CheckpointPhase::Prefetch,
            CheckpointPhase::Prefetch => CheckpointPhase::Backfill,
            CheckpointPhase::Backfill | CheckpointPhase::Done => CheckpointPhase::Done,
        };
        Self {
            phase,
            page_token: None,
            ..self
        }
    }
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;

    #[test]
    fn bodies_keep_the_inline_images_their_html_references() {
        let raw = include_bytes!("../../openmail-mime/tests/fixtures/newsletter-cid.eml");
        let raw = openmail_mime::encode_base64url(raw);
        let fetched_at = datetime!(2026-11-10 12:00 UTC);
        let body = parse_body(&MessageId::new("m"), Some(&raw), fetched_at).unwrap();
        let parts: Vec<(&str, &str)> = body
            .inline
            .iter()
            .map(|part| (part.content_id.as_str(), part.mime_type.as_str()))
            .collect();
        assert_eq!(
            parts,
            [
                ("chart.gif", "image/gif"),
                ("logo@news.example.com", "image/gif")
            ]
        );
        assert!(body.inline[1].data.starts_with(b"GIF89a"));
    }

    #[test]
    fn unreferenced_and_non_image_parts_are_not_kept() {
        let raw = "From: a@example.com\r\n\
            Content-Type: multipart/related; boundary=\"b\"\r\n\r\n\
            --b\r\nContent-Type: text/html\r\n\r\n\
            <img src=\"cid:png@x\"><img src=\"cid:doc@x\">\r\n\
            --b\r\nContent-Type: application/octet-stream\r\n\
            Content-ID: <png@x>\r\nContent-Transfer-Encoding: base64\r\n\r\n\
            iVBORw0KGgoAAAA=\r\n\
            --b\r\nContent-Type: application/pdf\r\n\
            Content-ID: <doc@x>\r\n\r\n%PDF-1.4\r\n\
            --b\r\nContent-Type: image/gif\r\n\
            Content-ID: <unused@x>\r\n\r\nGIF89a\r\n\
            --b--\r\n";
        let raw = openmail_mime::encode_base64url(raw.as_bytes());
        let body = parse_body(
            &MessageId::new("m"),
            Some(&raw),
            datetime!(2026-11-10 12:00 UTC),
        )
        .unwrap();
        // The generic part is sniffed as a PNG; the PDF and the image the HTML
        // doesn't mention are left out.
        let parts: Vec<(&str, &str)> = body
            .inline
            .iter()
            .map(|part| (part.content_id.as_str(), part.mime_type.as_str()))
            .collect();
        assert_eq!(parts, [("png@x", "image/png")]);
    }

    #[test]
    fn cutoff_resolves_to_unix_seconds() {
        let now = datetime!(2026-11-10 12:00 UTC);
        assert_eq!(Cutoff::All.after(now), None);
        assert_eq!(
            Cutoff::Days(90).after(now),
            Some(datetime!(2026-08-12 12:00 UTC).unix_timestamp())
        );
        assert_eq!(Cutoff::Since(now).after(now), Some(now.unix_timestamp()));
        assert_eq!(Cutoff::default(), Cutoff::Days(90));
    }

    #[test]
    fn checkpoints_round_trip_and_advance() {
        let start = Checkpoint {
            phase: CheckpointPhase::Inbox,
            page_token: None,
            after: Some(1_700_000_000),
            inbox_cutoff: true,
            sweep: true,
        };
        assert_eq!(
            start.encode(),
            r#"{"phase":"inbox","after":1700000000,"inbox_cutoff":true,"sweep":true}"#
        );
        let paged = start.clone().with_page_token("p2".to_owned());
        assert_eq!(Checkpoint::decode(&paged.encode()), Some(paged.clone()));

        let prefetch = paged.next_phase();
        assert_eq!(prefetch.phase, CheckpointPhase::Prefetch);
        assert_eq!(prefetch.page_token, None);
        let backfill = prefetch.next_phase();
        assert_eq!(backfill.phase, CheckpointPhase::Backfill);
        assert_eq!(backfill.after, Some(1_700_000_000));
        assert_eq!(backfill.next_phase().phase, CheckpointPhase::Done);
    }

    #[test]
    fn checkpoints_from_before_the_inbox_cutoff_restart_the_inbox_listing() {
        let old = Checkpoint::decode(r#"{"phase":"inbox","page_token":"p9","after":1700000000}"#)
            .unwrap();
        assert_eq!(old.phase, CheckpointPhase::Inbox);
        assert_eq!(old.page_token, None);
        assert!(old.inbox_cutoff);
        // Its earlier pages were not recorded, so nothing is swept.
        assert!(!old.sweep);

        // Without a cutoff the listing is the same, so the token stays valid.
        let all =
            Checkpoint::decode(r#"{"phase":"inbox","page_token":"p9","after":null}"#).unwrap();
        assert_eq!(all.page_token.as_deref(), Some("p9"));

        // The backfill always used the cutoff.
        let backfill =
            Checkpoint::decode(r#"{"phase":"backfill","page_token":"b9","after":1700000000}"#)
                .unwrap();
        assert_eq!(backfill.page_token.as_deref(), Some("b9"));
        assert!(backfill.inbox_cutoff);
    }

    #[test]
    fn unreadable_checkpoints_are_ignored() {
        assert_eq!(Checkpoint::decode("not json"), None);
        assert_eq!(Checkpoint::decode(r#"{"phase":"later"}"#), None);
        assert_eq!(Checkpoint::decode(r#"{"phase":"done","after":null}"#), None);
    }
}
