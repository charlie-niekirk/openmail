//! The replay worker: sending queued ops to Gmail.

use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;
use std::time::Duration;

use openmail_core::{Label, LabelId, MessageId, ThreadId};
use openmail_gmail::api::{self, LabelRequest, ModifyLabels, QuotaKind};
use openmail_store::{PendingOp, PendingOpState, Store};
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;

use super::effect::{self, Key, Record};
use super::{Op, OpEvent, OpId, OpsQueue, Target, load, save};
use crate::{Error, Result};

/// What one [`OpsQueue::flush`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Flush {
    /// Ops Gmail accepted.
    pub sent: usize,
    /// Ops dropped because a later op undid them before either was sent.
    pub cancelled_out: usize,
    /// Ops that failed and will be retried.
    pub retrying: usize,
    /// Ops Gmail rejected, dropped and rolled back.
    pub failed: usize,
    /// Ops still queued: not due yet, waiting to retry, or waiting for an
    /// earlier op on the same thread or label.
    pub waiting: usize,
    /// When the next waiting op is due, if one is waiting for a time.
    pub next_due: Option<OffsetDateTime>,
}

impl Flush {
    /// Whether ops left the queue, which may let waiting ones go.
    fn progressed(&self) -> bool {
        self.sent + self.cancelled_out + self.failed > 0
    }
}

/// A queued op loaded for sending.
struct Pending {
    id: OpId,
    row: PendingOp,
    record: Record,
    keys: BTreeSet<Key>,
}

/// How a failed send is handled.
enum Class {
    /// Retry after a backoff of at least this long.
    Transient(Option<Duration>),
    /// Gmail rejected the op: drop it and roll it back.
    Permanent,
    /// Nothing will get through until the user acts (sign in again): stop.
    Fatal,
}

fn classify(error: &api::Error, max_backoff: Duration) -> Class {
    use api::Error as E;
    match error {
        E::Quota {
            kind: QuotaKind::RateLimit,
            retry_after,
            ..
        } => Class::Transient(*retry_after),
        // Retrying today will not help; try again much later.
        E::Quota { .. } => Class::Transient(Some(max_backoff)),
        E::Api { status, .. } if status.is_server_error() || status.as_u16() == 408 => {
            Class::Transient(None)
        }
        E::Api { .. } | E::NotFound { .. } | E::Domain(_) => Class::Permanent,
        E::Unauthorized | E::Token(_) | E::InvalidConfig(_) | E::HistoryExpired { .. } => {
            Class::Fatal
        }
        _ => Class::Transient(None),
    }
}

/// Treats "not found" as done for deletes: the thing is gone either way.
fn gone_is_ok(result: api::Result<()>) -> api::Result<()> {
    match result {
        Err(api::Error::NotFound { .. }) => Ok(()),
        other => other,
    }
}

impl OpsQueue {
    /// Runs the replay worker until `cancel` fires: sends ops as they become
    /// due, sleeping in between (an [`enqueue`](Self::enqueue) or
    /// [`wake`](Self::wake) wakes it). Ops a previous worker left half-sent are
    /// sent again (sending is idempotent).
    ///
    /// Returns `Ok` when cancelled, and an error when nothing more can be sent
    /// until the user acts (signing in again) or the store fails; the queue is
    /// kept either way. `store` is borrowed mutably only so the future is
    /// `Send`.
    pub async fn run(
        &self,
        store: &mut Store,
        cancel: &CancellationToken,
        mut on_event: impl FnMut(OpEvent) + Send,
    ) -> Result<()> {
        self.replay(store, cancel, &mut on_event, false).await
    }

    /// Like [`run`](Self::run), but returns once no op is queued (sent ops
    /// waiting for history sync do not count). For the CLI.
    pub async fn drain(
        &self,
        store: &mut Store,
        cancel: &CancellationToken,
        mut on_event: impl FnMut(OpEvent) + Send,
    ) -> Result<()> {
        self.replay(store, cancel, &mut on_event, true).await
    }

    async fn replay(
        &self,
        store: &mut Store,
        cancel: &CancellationToken,
        on_event: &mut (impl FnMut(OpEvent) + Send),
        until_idle: bool,
    ) -> Result<()> {
        let released = store.release_sending_ops()?;
        if released > 0 {
            tracing::info!(released, "resending ops interrupted mid-send");
        }
        loop {
            let flush = match self.flush(store, cancel, on_event).await {
                Ok(flush) => flush,
                Err(Error::Cancelled) => return Ok(()),
                Err(err) => return Err(err),
            };
            if flush.progressed() {
                continue;
            }
            if until_idle && flush.waiting == 0 {
                return Ok(());
            }
            let wait = flush
                .next_due
                .map(|due| Duration::try_from(due - OffsetDateTime::now_utc()).unwrap_or_default());
            tokio::select! {
                biased;
                () = cancel.cancelled() => return Ok(()),
                () = self.wake.notified() => {}
                () = sleep(wait) => {}
            }
        }
    }

    /// Sends every op that is due now, once, and reports what happened.
    ///
    /// Ops touching a thread or label go out in queue order: an op waits
    /// while an earlier op on the same thread or label is waiting. Label
    /// changes with the same labels are sent together in `batchModify` calls.
    ///
    /// Fails, leaving unsent ops queued, if the store fails, `cancel` fires
    /// ([`Error::Cancelled`]), or Gmail refuses the account's credentials.
    pub async fn flush(
        &self,
        store: &mut Store,
        cancel: &CancellationToken,
        on_event: &mut (impl FnMut(OpEvent) + Send),
    ) -> Result<Flush> {
        let now = OffsetDateTime::now_utc();
        let mut flush = Flush::default();
        let mut blocked: HashSet<Key> = HashSet::new();
        let mut queued = Vec::new();
        for row in store.pending_ops()? {
            let record = load(&row.payload)?;
            let keys = record.keys();
            match row.state {
                PendingOpState::Queued => queued.push(Pending {
                    id: OpId(row.id),
                    row,
                    record,
                    keys,
                }),
                // Another worker is sending it.
                PendingOpState::Sending => blocked.extend(keys),
                PendingOpState::Sent => {}
            }
        }

        // Pick the ops to send now, in queue order.
        let mut taken = vec![false; queued.len()];
        let mut selected = Vec::new();
        let mut cancelled_out = Vec::new();
        for i in 0..queued.len() {
            if taken[i] {
                continue;
            }
            let op = &queued[i];
            let due = op.row.next_retry_at <= now;
            if due && op.keys.iter().all(|k| !blocked.contains(k)) && !op.record.needs_local_label()
            {
                match inverse_of(&queued, &taken, i) {
                    None => {
                        taken[i] = true;
                        selected.push(i);
                        continue;
                    }
                    Some(j) if queued[j].row.next_retry_at <= now => {
                        taken[i] = true;
                        taken[j] = true;
                        cancelled_out.push((i, j));
                        continue;
                    }
                    // Hold it until its inverse is due (or undone).
                    Some(_) => {}
                }
            }
            blocked.extend(op.keys.iter().cloned());
            flush.waiting += 1;
            if !due {
                flush.next_due = Some(
                    flush
                        .next_due
                        .map_or(op.row.next_retry_at, |t| t.min(op.row.next_retry_at)),
                );
            }
        }

        for (i, j) in cancelled_out {
            let (a, b) = (&queued[i], &queued[j]);
            let dropped = store.transaction(|store| {
                if !(store.claim_pending_op(a.id.0)? && store.claim_pending_op(b.id.0)?) {
                    // Undone meanwhile: leave both for the next flush.
                    return Err(Error::Cancelled);
                }
                store.delete_pending_op(a.id.0)?;
                store.delete_pending_op(b.id.0)?;
                Ok(())
            });
            match dropped {
                Ok(()) => {
                    tracing::debug!(a = %a.id, b = %b.id, "ops cancelled each other out");
                    for op in [a, b] {
                        flush.cancelled_out += 1;
                        on_event(OpEvent::CancelledOut {
                            id: op.id,
                            op: op.record.op.clone(),
                        });
                    }
                }
                Err(Error::Cancelled) => {}
                Err(err) => return Err(err),
            }
        }

        // Claim them, so an undo can no longer race the send.
        let mut claimed = Vec::new();
        for i in selected {
            if store.claim_pending_op(queued[i].id.0)? {
                claimed.push(i);
            }
        }

        let units = plan(&queued, &claimed);
        let mut failed_keys: HashSet<Key> = HashSet::new();
        for (n, unit) in units.iter().enumerate() {
            // An earlier op on the same thread failed: keep the order.
            let (ready, held): (Vec<usize>, Vec<usize>) = unit
                .iter()
                .partition(|&&i| queued[i].keys.iter().all(|k| !failed_keys.contains(k)));
            for &i in &held {
                store.requeue_pending_op(queued[i].id.0, now, None, false)?;
                flush.waiting += 1;
            }
            if ready.is_empty() {
                continue;
            }
            let result = tokio::select! {
                biased;
                () = cancel.cancelled() => None,
                result = self.send(&queued, &ready) => Some(result),
            };
            let Some(result) = result else {
                release(store, &queued, units[n..].iter().flatten())?;
                return Err(Error::Cancelled);
            };
            // Takes the store as an argument: a closure holding `&Store`
            // across the `await` below would make the future not `Send`.
            let mut settle = |store: &Store, unit: &[usize], result| {
                self.settle(
                    store,
                    &queued,
                    unit,
                    result,
                    &mut failed_keys,
                    &mut flush,
                    on_event,
                )
            };
            let outcome = match settle(store, &ready, result) {
                // Gmail rejected several ops together: find the culprit by
                // sending them one at a time.
                Ok(true) => {
                    let mut outcome = Ok(false);
                    for &i in &ready {
                        let result = self.send(&queued, &[i]).await;
                        outcome = settle(store, &[i], result);
                        if outcome.is_err() {
                            break;
                        }
                    }
                    outcome
                }
                outcome => outcome,
            };
            if let Err(err) = outcome {
                release(store, &queued, units[n..].iter().flatten())?;
                return Err(err);
            }
        }
        Ok(flush)
    }

    /// Sends a unit of ops (a group of label changes with the same labels, or
    /// one other op). Returns the label Gmail created, for a label creation.
    async fn send(&self, queued: &[Pending], unit: &[usize]) -> api::Result<Option<Label>> {
        let client = &self.client;
        let first = &queued[unit[0]].record;
        match &first.op {
            Op::ModifyLabels { add, remove, .. } => {
                let labels = ModifyLabels {
                    add_label_ids: add.clone(),
                    remove_label_ids: remove.clone(),
                };
                if labels.is_empty() {
                    return Ok(None);
                }
                let mut messages: Vec<MessageId> = Vec::new();
                let mut threads: Vec<ThreadId> = Vec::new();
                for &i in unit {
                    let effect = &queued[i].record.effect;
                    let ids = effect.messages.iter().map(|m| &m.id);
                    for id in ids.chain(&effect.unresolved_messages) {
                        if !messages.contains(id) {
                            messages.push(id.clone());
                        }
                    }
                    for id in &effect.unresolved_threads {
                        if !threads.contains(id) {
                            threads.push(id.clone());
                        }
                    }
                }
                if !messages.is_empty() {
                    client.batch_modify_messages(&messages, &labels).await?;
                }
                for thread in &threads {
                    client.modify_thread(thread, &labels).await?;
                }
                Ok(None)
            }
            Op::Trash { target } => {
                match target {
                    Target::Threads(ids) => {
                        for id in ids {
                            client.trash_thread(id).await?;
                        }
                    }
                    Target::Messages(ids) => {
                        for id in ids {
                            client.trash_message(id).await?;
                        }
                    }
                }
                Ok(None)
            }
            Op::Untrash { target } => {
                match target {
                    Target::Threads(ids) => {
                        for id in ids {
                            client.untrash_thread(id).await?;
                        }
                    }
                    Target::Messages(ids) => {
                        for id in ids {
                            client.untrash_message(id).await?;
                        }
                    }
                }
                Ok(None)
            }
            Op::Delete { target } => {
                match target {
                    Target::Threads(ids) => {
                        for id in ids {
                            gone_is_ok(client.delete_thread(id).await)?;
                        }
                    }
                    Target::Messages(ids) => {
                        for id in ids {
                            gone_is_ok(client.delete_message(id).await)?;
                        }
                    }
                }
                Ok(None)
            }
            Op::CreateLabel { name, color } => {
                let mut request = LabelRequest::new().name(name.clone());
                if let Some(color) = color {
                    request = request.color(&color.text_color, &color.background_color);
                }
                let label = client.create_label(&request).await?;
                Ok(Some(Label::from(label)))
            }
            Op::RenameLabel { label, name } => {
                client
                    .patch_label(label, &LabelRequest::new().name(name.clone()))
                    .await?;
                Ok(None)
            }
            Op::RecolorLabel { label, color } => {
                let request = LabelRequest::new().color(&color.text_color, &color.background_color);
                client.patch_label(label, &request).await?;
                Ok(None)
            }
            Op::DeleteLabel { label } => {
                gone_is_ok(client.delete_label(label).await)?;
                Ok(None)
            }
        }
    }

    /// Records the outcome of sending a unit: marks its ops sent, queues them
    /// to retry, or drops and rolls back an op Gmail rejected. Returns `true`
    /// if Gmail rejected a unit of several ops, which should then be sent one
    /// at a time, and an error if nothing more can be sent.
    #[allow(clippy::too_many_arguments)] // The flush's state, passed along.
    fn settle(
        &self,
        store: &Store,
        queued: &[Pending],
        unit: &[usize],
        result: api::Result<Option<Label>>,
        failed_keys: &mut HashSet<Key>,
        flush: &mut Flush,
        on_event: &mut (impl FnMut(OpEvent) + Send),
    ) -> Result<bool> {
        let error = match result {
            Ok(created) => {
                self.sent(store, queued, unit, created, flush, on_event)?;
                return Ok(false);
            }
            Err(error) => error,
        };
        match classify(&error, self.config.max_backoff) {
            Class::Transient(retry_after) => {
                let error = Arc::new(error);
                for &i in unit {
                    let op = &queued[i];
                    self.retry(store, op, retry_after, &error, flush, on_event)?;
                    // Later ops on the same threads wait for it.
                    failed_keys.extend(op.keys.iter().cloned());
                }
            }
            Class::Permanent if unit.len() > 1 => return Ok(true),
            Class::Permanent => fail(store, &queued[unit[0]], error, flush, on_event)?,
            Class::Fatal => return Err(error.into()),
        }
        Ok(false)
    }

    /// Records ops Gmail accepted.
    fn sent(
        &self,
        store: &Store,
        queued: &[Pending],
        unit: &[usize],
        created: Option<Label>,
        flush: &mut Flush,
        on_event: &mut (impl FnMut(OpEvent) + Send),
    ) -> Result<()> {
        let now = OffsetDateTime::now_utc();
        store.transaction(|store| {
            if let (Some(label), Some(local)) =
                (&created, &queued[unit[0]].record.effect.local_label)
            {
                adopt_label(store, local, label)?;
            }
            for &i in unit {
                store.mark_pending_op_sent(queued[i].id.0, now)?;
            }
            Ok::<_, Error>(())
        })?;
        for &i in unit {
            let op = &queued[i];
            tracing::debug!(id = %op.id, kind = op.record.op.kind(), "sent an op");
            flush.sent += 1;
            on_event(OpEvent::Sent {
                id: op.id,
                op: op.record.op.clone(),
            });
        }
        Ok(())
    }

    /// Queues an op again after a transient failure.
    fn retry(
        &self,
        store: &Store,
        op: &Pending,
        retry_after: Option<Duration>,
        error: &Arc<api::Error>,
        flush: &mut Flush,
        on_event: &mut (impl FnMut(OpEvent) + Send),
    ) -> Result<()> {
        let attempts = op.row.attempts + 1;
        let retry_at = OffsetDateTime::now_utc() + self.config.backoff(attempts, retry_after);
        store.requeue_pending_op(op.id.0, retry_at, Some(&error_chain(error)), true)?;
        tracing::debug!(id = %op.id, attempts, %retry_at, error = %error, "op failed; retrying");
        flush.retrying += 1;
        flush.waiting += 1;
        flush.next_due = Some(flush.next_due.map_or(retry_at, |t| t.min(retry_at)));
        on_event(OpEvent::Retrying {
            id: op.id,
            op: op.record.op.clone(),
            attempts,
            retry_at,
            error: Arc::clone(error),
        });
        Ok(())
    }
}

/// The first later op that exactly undoes op `i`, unless an op on the same
/// thread or label comes between them.
fn inverse_of(queued: &[Pending], taken: &[bool], i: usize) -> Option<usize> {
    let op = &queued[i];
    for (j, later) in queued.iter().enumerate().skip(i + 1) {
        if taken[j] || later.keys.is_disjoint(&op.keys) {
            continue;
        }
        return op.record.is_inverted_by(&later.record).then_some(j);
    }
    None
}

/// Groups the claimed ops into units sent in order. Consecutive label changes
/// share a unit when they change the same labels, as long as no thread is in
/// two units of the same round (so units in a round are independent); other
/// ops are units of their own.
fn plan(queued: &[Pending], claimed: &[usize]) -> Vec<Vec<usize>> {
    let mut units = Vec::new();
    let mut round: Vec<(&Op, Vec<usize>)> = Vec::new();
    let mut round_keys: HashSet<&Key> = HashSet::new();
    let flush_round = |round: &mut Vec<(&Op, Vec<usize>)>, units: &mut Vec<Vec<usize>>| {
        units.extend(round.drain(..).map(|(_, unit)| unit));
    };
    for &i in claimed {
        let op = &queued[i];
        if !matches!(op.record.op, Op::ModifyLabels { .. }) {
            flush_round(&mut round, &mut units);
            round_keys.clear();
            units.push(vec![i]);
            continue;
        }
        if op.keys.iter().any(|k| round_keys.contains(k)) {
            flush_round(&mut round, &mut units);
            round_keys.clear();
        }
        round_keys.extend(op.keys.iter());
        match round
            .iter_mut()
            .find(|(first, _)| same_labels(first, &op.record.op))
        {
            Some((_, unit)) => unit.push(i),
            None => round.push((&op.record.op, vec![i])),
        }
    }
    flush_round(&mut round, &mut units);
    units
}

/// Whether two label changes add and remove the same labels.
fn same_labels(a: &Op, b: &Op) -> bool {
    match (label_sets(a), label_sets(b)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

type LabelSets<'a> = (BTreeSet<&'a LabelId>, BTreeSet<&'a LabelId>);

fn label_sets(op: &Op) -> Option<LabelSets<'_>> {
    match op {
        Op::ModifyLabels { add, remove, .. } => {
            Some((add.iter().collect(), remove.iter().collect()))
        }
        _ => None,
    }
}

/// Puts claimed ops back in the queue without counting an attempt.
fn release<'a>(
    store: &Store,
    queued: &[Pending],
    ops: impl Iterator<Item = &'a usize>,
) -> Result<()> {
    let now = OffsetDateTime::now_utc();
    for &i in ops {
        let op = &queued[i];
        if store.pending_op(op.id.0)?.map(|r| r.state) == Some(PendingOpState::Sending) {
            store.requeue_pending_op(op.id.0, op.row.next_retry_at.min(now), None, false)?;
        }
    }
    Ok(())
}

/// Drops an op Gmail rejected and rolls it back.
fn fail(
    store: &Store,
    op: &Pending,
    error: api::Error,
    flush: &mut Flush,
    on_event: &mut (impl FnMut(OpEvent) + Send),
) -> Result<()> {
    tracing::warn!(id = %op.id, kind = op.record.op.kind(), %error, "Gmail rejected an op; rolling it back");
    let dependents = store.transaction(|store| roll_back(store, op.id, &op.record))?;
    let error = Arc::new(error);
    for (id, op) in std::iter::once((op.id, op.record.op.clone())).chain(dependents) {
        flush.failed += 1;
        on_event(OpEvent::Failed {
            id,
            op,
            error: Arc::clone(&error),
        });
    }
    Ok(())
}

/// Reverts an op's local change and removes it from the queue. For a label
/// creation, the queued ops that use the label go too, newest first. Returns
/// those.
pub(super) fn roll_back(store: &Store, id: OpId, record: &Record) -> Result<Vec<(OpId, Op)>> {
    let mut dependents = Vec::new();
    if let Some(local) = &record.effect.local_label {
        for row in store.pending_ops()?.into_iter().rev() {
            if row.id == id.0 || row.state == PendingOpState::Sent {
                continue;
            }
            let later = load(&row.payload)?;
            if later.label_ids().contains(&local) {
                effect::revert(store, &later.effect)?;
                store.delete_pending_op(row.id)?;
                dependents.push((OpId(row.id), later.op));
            }
        }
    }
    effect::revert(store, &record.effect)?;
    store.delete_pending_op(id.0)?;
    Ok(dependents)
}

/// Swaps a label created offline for the one Gmail created, in the store
/// and in the queued ops that use it.
fn adopt_label(store: &Store, local: &LabelId, label: &Label) -> Result<()> {
    store.replace_label_id(local, label)?;
    for row in store.pending_ops()? {
        if row.state == PendingOpState::Sent {
            continue;
        }
        let mut record = load(&row.payload)?;
        if record.replace_label_id(local, &label.id) {
            save(store, OpId(row.id), &record)?;
        }
    }
    Ok(())
}

/// Re-applies queued ops on top of what sync just wrote for `threads` (and
/// for labels, if `labels_synced`), so the user's pending changes still show.
/// Data fetched from `fetched_since` on reflects ops sent before then; ops
/// sent later are re-applied too. Call it in the transaction that wrote the
/// sync's changes. Returns how many ops were re-applied.
pub(crate) fn rebase(
    store: &Store,
    threads: &HashSet<&ThreadId>,
    labels_synced: bool,
    fetched_since: OffsetDateTime,
) -> Result<usize> {
    let mut reapplied = 0;
    for row in store.pending_ops()? {
        let mut record = load(&row.payload)?;
        let label_op = matches!(
            record.op,
            Op::CreateLabel { .. }
                | Op::RenameLabel { .. }
                | Op::RecolorLabel { .. }
                | Op::DeleteLabel { .. }
        );
        let live = match row.state {
            PendingOpState::Queued | PendingOpState::Sending => true,
            // A sent label op's label is Gmail's from then on.
            PendingOpState::Sent => !label_op && row.sent_at.is_some_and(|t| t > fetched_since),
        };
        let touched = record.threads().any(|t| threads.contains(t))
            || (labels_synced && (label_op || !record.label_ids().is_empty()));
        if !(live && touched) {
            continue;
        }
        let id = OpId(row.id);
        record.effect = effect::apply(store, id, &record.op, Some(&record.effect))?;
        save(store, id, &record)?;
        reapplied += 1;
    }
    if reapplied > 0 {
        tracing::debug!(reapplied, "re-applied queued ops on top of synced changes");
    }
    Ok(reapplied)
}

/// Forgets sent ops that history sync has caught up with: those sent before
/// `fetched_since`, when a history sync that started then has finished.
pub(crate) fn forget_sent(store: &Store, fetched_since: OffsetDateTime) -> Result<usize> {
    Ok(store.delete_sent_ops(fetched_since)?)
}

/// An error and its sources, joined with `: `.
fn error_chain(err: &dyn std::error::Error) -> String {
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(err) = source {
        text.push_str(": ");
        text.push_str(&err.to_string());
        source = err.source();
    }
    text
}

/// Sleeps for `duration`, or forever if `None`.
async fn sleep(duration: Option<Duration>) {
    match duration {
        Some(duration) => tokio::time::sleep(duration).await,
        None => std::future::pending().await,
    }
}
