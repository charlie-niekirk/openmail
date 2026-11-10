//! The offline ops queue: user actions applied to the store at once, then
//! sent to Gmail in the background.
//!
//! [`OpsQueue::enqueue`] applies an [`Op`] to the local store optimistically,
//! in the same transaction that appends it to the persisted `pending_ops`
//! queue, so the store's queries show the change immediately. After an undo
//! window ([`OpsConfig::undo_window`], in which [`OpsQueue::undo`] cancels it
//! and reverts the local change), the replay worker ([`OpsQueue::run`]) sends
//! it:
//!
//! - in queue order for each thread or label it touches;
//! - label changes with the same labels are coalesced into
//!   `messages.batchModify` calls, and a label change followed by its exact
//!   inverse (star, then unstar) cancels out without either being sent;
//! - transient failures (network, Gmail server errors, rate limits) are
//!   retried with exponential backoff, and requests go through the client's
//!   quota limiter;
//! - an op Gmail rejects (a 4xx other than a rate limit or a sign-in problem)
//!   is dropped and its local change reverted;
//! - sign-in problems stop the worker with an error, keeping the queue.
//!
//! The queue survives restarts: [`OpsQueue::run`] picks up where it stopped.
//!
//! **Conflicts: the server wins.** When incremental (or initial) sync writes
//! Gmail's version of a thread that has unsent ops, those ops are applied
//! again on top of it, so the user's pending change shows until it is sent.
//! A sent op is kept for that purpose until a history sync started after it
//! was sent has finished; after that, Gmail's state is taken as is.

mod effect;
mod replay;

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use openmail_core::{LabelColor, LabelId, MessageId, SystemLabel, ThreadId};
use openmail_gmail::GmailClient;
use openmail_gmail::api;
use openmail_store::{PendingOpState, Store};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::sync::Notify;

use crate::Result;
use effect::Record;

pub use replay::Flush;

/// What an op applies to.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    /// Every message of these threads.
    Threads(Vec<ThreadId>),
    /// These messages.
    Messages(Vec<MessageId>),
}

impl Target {
    /// One thread.
    pub fn thread(id: impl Into<ThreadId>) -> Self {
        Self::Threads(vec![id.into()])
    }

    /// One message.
    pub fn message(id: impl Into<MessageId>) -> Self {
        Self::Messages(vec![id.into()])
    }
}

/// A change to the mailbox, made locally first and then sent to Gmail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Op {
    /// Adds and removes labels: archive (remove `INBOX`), read and unread
    /// (`UNREAD`), star (`STARRED`), important (`IMPORTANT`), spam (add
    /// `SPAM`, remove `INBOX`) and "move to" a label.
    ModifyLabels {
        target: Target,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        add: Vec<LabelId>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        remove: Vec<LabelId>,
    },
    /// Moves to Trash (locally: adds `TRASH`, removes `INBOX`).
    Trash { target: Target },
    /// Takes out of Trash (locally: removes `TRASH`).
    Untrash { target: Target },
    /// Deletes permanently, skipping Trash.
    Delete { target: Target },
    /// Creates a user label. Until Gmail assigns its ID, the label has a
    /// temporary local one ([`OpsQueue::local_label`]), which later ops may
    /// use; they are sent once the label exists.
    CreateLabel {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        color: Option<LabelColor>,
    },
    /// Renames a user label (nest with `/`).
    RenameLabel { label: LabelId, name: String },
    /// Changes a user label's colors.
    RecolorLabel { label: LabelId, color: LabelColor },
    /// Deletes a user label, removing it from every message.
    DeleteLabel { label: LabelId },
}

impl Op {
    /// Adds and removes labels.
    pub fn modify(
        target: Target,
        add: impl IntoIterator<Item = LabelId>,
        remove: impl IntoIterator<Item = LabelId>,
    ) -> Self {
        Self::ModifyLabels {
            target,
            add: add.into_iter().collect(),
            remove: remove.into_iter().collect(),
        }
    }

    fn system(target: Target, add: &[SystemLabel], remove: &[SystemLabel]) -> Self {
        Self::modify(
            target,
            add.iter().map(|&l| LabelId::from(l)),
            remove.iter().map(|&l| LabelId::from(l)),
        )
    }

    /// Archives: removes `INBOX`.
    pub fn archive(target: Target) -> Self {
        Self::system(target, &[], &[SystemLabel::Inbox])
    }

    /// Moves back to the Inbox: adds `INBOX`.
    pub fn move_to_inbox(target: Target) -> Self {
        Self::system(target, &[SystemLabel::Inbox], &[])
    }

    /// Marks read: removes `UNREAD`.
    pub fn mark_read(target: Target) -> Self {
        Self::system(target, &[], &[SystemLabel::Unread])
    }

    /// Marks unread: adds `UNREAD`.
    pub fn mark_unread(target: Target) -> Self {
        Self::system(target, &[SystemLabel::Unread], &[])
    }

    /// Stars: adds `STARRED`.
    pub fn star(target: Target) -> Self {
        Self::system(target, &[SystemLabel::Starred], &[])
    }

    /// Unstars: removes `STARRED`.
    pub fn unstar(target: Target) -> Self {
        Self::system(target, &[], &[SystemLabel::Starred])
    }

    /// Marks important: adds `IMPORTANT`.
    pub fn mark_important(target: Target) -> Self {
        Self::system(target, &[SystemLabel::Important], &[])
    }

    /// Marks not important: removes `IMPORTANT`.
    pub fn mark_not_important(target: Target) -> Self {
        Self::system(target, &[], &[SystemLabel::Important])
    }

    /// Reports spam: adds `SPAM`, removes `INBOX`.
    pub fn spam(target: Target) -> Self {
        Self::system(target, &[SystemLabel::Spam], &[SystemLabel::Inbox])
    }

    /// Not spam: removes `SPAM`, adds `INBOX`.
    pub fn not_spam(target: Target) -> Self {
        Self::system(target, &[SystemLabel::Inbox], &[SystemLabel::Spam])
    }

    /// The op's kind, as stored in `pending_ops.op_type`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::ModifyLabels { .. } => "modify_labels",
            Self::Trash { .. } => "trash",
            Self::Untrash { .. } => "untrash",
            Self::Delete { .. } => "delete",
            Self::CreateLabel { .. } => "create_label",
            Self::RenameLabel { .. } => "rename_label",
            Self::RecolorLabel { .. } => "recolor_label",
            Self::DeleteLabel { .. } => "delete_label",
        }
    }
}

/// A queued op's stable ID, for [`OpsQueue::undo`]. IDs are never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OpId(i64);

impl OpId {
    /// The ID as stored.
    pub fn get(self) -> i64 {
        self.0
    }
}

impl From<i64> for OpId {
    fn from(id: i64) -> Self {
        Self(id)
    }
}

impl fmt::Display for OpId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// What happened to a queued op, reported by the replay worker.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum OpEvent {
    /// Gmail accepted the op.
    Sent { id: OpId, op: Op },
    /// A later op exactly undid this one before either was sent, so neither
    /// was (both are reported).
    CancelledOut { id: OpId, op: Op },
    /// Sending failed for a reason that may pass; it is retried at
    /// `retry_at`. `attempts` counts the failures so far.
    Retrying {
        id: OpId,
        op: Op,
        attempts: u32,
        retry_at: OffsetDateTime,
        error: Arc<api::Error>,
    },
    /// Gmail rejected the op. It was dropped and its local change reverted
    /// (with any later ops that used the label it was creating).
    Failed {
        id: OpId,
        op: Op,
        error: Arc<api::Error>,
    },
}

/// A queued op, for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedOp {
    pub id: OpId,
    pub op: Op,
    pub state: PendingOpState,
    /// Failed attempts so far.
    pub attempts: u32,
    /// When it will be sent (or retried).
    pub next_attempt: OffsetDateTime,
    pub last_error: Option<String>,
}

/// How the queue sends ops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpsConfig {
    /// How long a new op waits before it is sent, during which
    /// [`OpsQueue::undo`] can cancel it (5 s by default).
    pub undo_window: Duration,
    /// The wait before the first retry; it doubles with each failure.
    pub initial_backoff: Duration,
    /// The longest wait between retries.
    pub max_backoff: Duration,
}

impl Default for OpsConfig {
    fn default() -> Self {
        Self {
            undo_window: Duration::from_secs(5),
            initial_backoff: Duration::from_secs(2),
            max_backoff: Duration::from_secs(5 * 60),
        }
    }
}

impl OpsConfig {
    /// The wait before retrying after `attempts` failures, at least
    /// `retry_after` if Gmail asked for one.
    pub fn backoff(&self, attempts: u32, retry_after: Option<Duration>) -> Duration {
        let doublings = attempts.saturating_sub(1).min(20);
        let backoff = self
            .initial_backoff
            .saturating_mul(1 << doublings)
            .min(self.max_backoff);
        retry_after.map_or(backoff, |after| backoff.max(after))
    }
}

/// The offline ops queue of one account. Cheap to clone: clones share the
/// client and wake each other's workers.
///
/// The store is passed to each call, so the UI side can enqueue on one
/// connection while the worker sends on another (the database is in WAL mode,
/// and writes are serialized).
#[derive(Debug, Clone)]
pub struct OpsQueue {
    client: GmailClient,
    config: OpsConfig,
    wake: Arc<Notify>,
}

impl OpsQueue {
    /// A queue sending through `client`.
    pub fn new(client: GmailClient, config: OpsConfig) -> Self {
        Self {
            client,
            config,
            wake: Arc::new(Notify::new()),
        }
    }

    /// The queue's settings.
    pub fn config(&self) -> &OpsConfig {
        &self.config
    }

    /// Applies `op` to the store and queues it, in one transaction. The
    /// store's queries reflect the change when this returns. It is sent after
    /// the undo window.
    pub fn enqueue(&self, store: &Store, op: Op) -> Result<OpId> {
        let now = OffsetDateTime::now_utc();
        let id = store.transaction(|store| {
            let id = OpId(store.insert_pending_op(
                op.kind(),
                "{}",
                now,
                now + self.config.undo_window,
            )?);
            let effect = effect::apply(store, id, &op, None)?;
            save(store, id, &Record { op, effect })?;
            Ok::<_, crate::Error>(id)
        })?;
        tracing::debug!(%id, "queued an op");
        self.wake();
        Ok(id)
    }

    /// Cancels an op that has not been sent, within the undo window, and
    /// reverts its local change. Undoing a label creation also undoes the
    /// queued ops that use the label.
    ///
    /// Returns `false`, changing nothing, if the op is unknown, being sent or
    /// sent, or its undo window has passed.
    pub fn undo(&self, store: &Store, id: OpId) -> Result<bool> {
        let now = OffsetDateTime::now_utc();
        store.transaction(|store| {
            let Some(row) = store.pending_op(id.0)? else {
                return Ok(false);
            };
            if row.state != PendingOpState::Queued
                || now >= row.created_at + self.config.undo_window
            {
                return Ok(false);
            }
            let record = load(&row.payload)?;
            replay::roll_back(store, id, &record)?;
            Ok::<_, crate::Error>(true)
        })
    }

    /// The temporary ID of the label a queued [`Op::CreateLabel`] is creating,
    /// which later ops can use until Gmail assigns the real one. `None` once
    /// it has been sent (the store then has the real ID) or if `id` is not a
    /// label creation.
    pub fn local_label(&self, store: &Store, id: OpId) -> Result<Option<LabelId>> {
        let Some(row) = store.pending_op(id.0)? else {
            return Ok(None);
        };
        if row.state == PendingOpState::Sent {
            return Ok(None);
        }
        Ok(load(&row.payload)?.effect.local_label)
    }

    /// Every op in the queue, oldest first, including sent ops kept until
    /// history sync has caught up.
    pub fn queued(&self, store: &Store) -> Result<Vec<QueuedOp>> {
        store
            .pending_ops()?
            .into_iter()
            .map(|row| {
                Ok(QueuedOp {
                    id: OpId(row.id),
                    op: load(&row.payload)?.op,
                    state: row.state,
                    attempts: row.attempts,
                    next_attempt: row.next_retry_at,
                    last_error: row.last_error,
                })
            })
            .collect()
    }

    /// Wakes the worker, for example after enqueuing through another clone.
    pub fn wake(&self) {
        self.wake.notify_one();
    }
}

/// Decodes a stored op.
fn load(payload: &str) -> Result<Record> {
    serde_json::from_str(payload)
        .map_err(|err| openmail_store::Error::Corrupt(format!("queued op: {err}")).into())
}

/// Stores an op's record as its payload.
fn save(store: &Store, id: OpId, record: &Record) -> Result<()> {
    let payload = serde_json::to_string(record)
        .map_err(|err| openmail_store::Error::Corrupt(format!("queued op: {err}")))?;
    store.set_pending_op_payload(id.0, &payload)?;
    Ok(())
}

pub(crate) use replay::{forget_sent, rebase};

#[cfg(test)]
mod tests;
