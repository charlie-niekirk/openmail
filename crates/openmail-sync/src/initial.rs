//! The initial sync: labels, then Inbox threads, then a backfill of older mail.

use std::future::Future;

use openmail_core::{Label, LabelId, MessageId, SystemLabel, ThreadId};
use openmail_gmail::GmailClient;
use openmail_gmail::api::{
    self, ApiThread, ListRequest, MAX_BATCH_SIZE, MessageFormat, ThreadFormat,
};
use openmail_mime::ParsedMessage;
use openmail_store::{MessageBody, Store, SyncState};
use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};
use tokio_util::sync::CancellationToken;

use crate::{Error, Result};

/// The headers kept with thread metadata during the initial sync.
pub const SYNC_HEADERS: &[&str] = &["From", "To", "Cc", "Subject", "Date", "Message-ID"];

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
}

impl Default for InitialSyncConfig {
    fn default() -> Self {
        Self {
            cutoff: Cutoff::default(),
            page_size: 100,
            prefetch_inbox_bodies: None,
            rate_limit_pause: std::time::Duration::from_secs(60),
            max_rate_limit_pauses: 5,
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
    /// The quota units per second the client allows itself right now (it
    /// adapts to Gmail's rate limiting unless fixed); `None` if unlimited.
    pub units_per_second: Option<u32>,
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
#[derive(Debug)]
pub struct InitialSync {
    client: GmailClient,
    store: Store,
    config: InitialSyncConfig,
}

impl InitialSync {
    /// A sync of the account `client` is signed in to, into `store`.
    pub fn new(client: GmailClient, store: Store, config: InitialSyncConfig) -> Self {
        Self {
            client,
            store,
            config,
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
                ..report.clone()
            }
        };
        let mut progress = |report: &Progress| progress(&with_stats(report));
        let mut report = Progress::default();
        let mut pauses = 0;
        let mut at_last_pause = report.advanced();
        let mut first = true;
        loop {
            let err = match self
                .run_once(cancel, first, &mut report, &mut progress)
                .await
            {
                Ok(()) => return Ok(with_stats(&report)),
                Err(err) => err,
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
        let labels: Vec<Label> = cancellable(cancel, self.client.list_labels())
            .await?
            .into_iter()
            .map(Label::from)
            .collect();
        self.store.replace_labels(&labels)?;
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
            let page = cancellable(cancel, self.client.list_threads(&request)).await?;
            let listed: Vec<ThreadId> = page.threads.iter().map(|t| t.id.clone()).collect();
            let fetch = self.changed_threads(page.threads)?;
            let (threads, gone) = self.fetch_threads(cancel, &fetch).await?;

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
                Ok::<_, Error>(deleted + save_checkpoint(store, &checkpoint)?)
            })?;

            report.threads += threads.len() as u64;
            report.messages += threads.iter().map(|(_, m)| m.len() as u64).sum::<u64>();
            report.skipped_threads += (listed.len() - threads.len()) as u64;
            report.deleted_threads += deleted;
            progress(report);
        }
        Ok(checkpoint)
    }

    /// The listed threads whose stored copy is missing or has another history ID.
    fn changed_threads(&self, listed: Vec<ApiThread>) -> Result<Vec<ThreadId>> {
        let ids: Vec<ThreadId> = listed.iter().map(|t| t.id.clone()).collect();
        let stored = self.store.thread_history_ids(&ids)?;
        Ok(listed
            .into_iter()
            .filter(|t| t.history_id.is_none() || t.history_id != stored.get(&t.id).copied())
            .map(|t| t.id)
            .collect())
    }

    /// Fetches thread metadata in batches. Threads deleted since they were
    /// listed are returned separately.
    async fn fetch_threads(
        &mut self,
        cancel: &CancellationToken,
        ids: &[ThreadId],
    ) -> Result<FetchedThreads> {
        fetch_threads(&self.client, cancel, ids).await
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
fn parse_body(
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
            html: parsed.html,
            fetched_at,
        }),
        Err(err) => {
            tracing::warn!(message = %id, %err, "could not parse a message body");
            None
        }
    }
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
