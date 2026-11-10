//! Rows of the offline ops queue (`pending_ops`) and the local edits ops make.
//!
//! The store treats an op's payload as opaque JSON; `openmail-sync` defines the
//! ops and decides what they change.

use std::collections::BTreeSet;

use openmail_core::{Label, LabelId, MessageId};
use rusqlite::{OptionalExtension as _, Row, named_params};
use time::OffsetDateTime;

use crate::convert::{date_from_sql, date_to_sql, u64_from_sql};
use crate::{Error, Result, Store};

/// Where a queued op is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PendingOpState {
    /// Waiting to be sent, or to be retried.
    Queued,
    /// Claimed by the replay worker and being sent. It can no longer be undone.
    Sending,
    /// Sent. Kept until history sync has caught up past it.
    Sent,
}

impl PendingOpState {
    fn from_sql(value: &str) -> Result<Self> {
        match value {
            "queued" => Ok(Self::Queued),
            "sending" => Ok(Self::Sending),
            "sent" => Ok(Self::Sent),
            other => Err(Error::Corrupt(format!("unknown op state {other:?}"))),
        }
    }
}

/// A `pending_ops` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingOp {
    /// A stable ID, never reused, which also orders the queue.
    pub id: i64,
    /// The op's kind, for inspection; the payload says everything.
    pub op_type: String,
    /// The op, as JSON.
    pub payload: String,
    pub state: PendingOpState,
    /// Failed attempts to send it so far.
    pub attempts: u32,
    /// The earliest time to send it (or send it again).
    pub next_retry_at: OffsetDateTime,
    pub created_at: OffsetDateTime,
    /// When it was sent, once it has been.
    pub sent_at: Option<OffsetDateTime>,
    /// Why the last attempt failed.
    pub last_error: Option<String>,
}

/// Labels a change actually added to or removed from a message: those it
/// already had (or lacked) are not listed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LabelDelta {
    pub added: Vec<LabelId>,
    pub removed: Vec<LabelId>,
}

impl LabelDelta {
    /// Whether the change did nothing.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }
}

const SELECT_OP: &str = "SELECT id, op_type, payload, state, attempts, next_retry_at, \
                         created_at, sent_at, last_error FROM pending_ops";

impl Store {
    /// Appends an op to the queue, to be sent no earlier than `not_before`.
    /// Returns its ID.
    pub fn insert_pending_op(
        &self,
        op_type: &str,
        payload: &str,
        created_at: OffsetDateTime,
        not_before: OffsetDateTime,
    ) -> Result<i64> {
        self.conn().execute(
            "INSERT INTO pending_ops (op_type, payload, next_retry_at, created_at)
             VALUES (:type, :payload, :next, :created)",
            named_params! {
                ":type": op_type,
                ":payload": payload,
                ":next": date_to_sql(not_before)?,
                ":created": date_to_sql(created_at)?,
            },
        )?;
        Ok(self.conn().last_insert_rowid())
    }

    /// Every queued, sending and sent op, oldest first.
    pub fn pending_ops(&self) -> Result<Vec<PendingOp>> {
        let mut stmt = self
            .conn()
            .prepare_cached(&format!("{SELECT_OP} ORDER BY id"))?;
        let rows = stmt
            .query_map([], OpRow::read)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(OpRow::into_op).collect()
    }

    /// The op with this ID, if it is still in the queue.
    pub fn pending_op(&self, id: i64) -> Result<Option<PendingOp>> {
        self.conn()
            .query_row(&format!("{SELECT_OP} WHERE id = ?1"), [id], OpRow::read)
            .optional()?
            .map(OpRow::into_op)
            .transpose()
    }

    /// Replaces an op's payload. Returns whether the op is in the queue.
    pub fn set_pending_op_payload(&self, id: i64, payload: &str) -> Result<bool> {
        let updated = self.conn().execute(
            "UPDATE pending_ops SET payload = ?2 WHERE id = ?1",
            rusqlite::params![id, payload],
        )?;
        Ok(updated > 0)
    }

    /// Marks a queued op as being sent. Returns `false`, changing nothing, if
    /// it is not queued (it was undone, or is already being sent).
    pub fn claim_pending_op(&self, id: i64) -> Result<bool> {
        let updated = self.conn().execute(
            "UPDATE pending_ops SET state = 'sending' WHERE id = ?1 AND state = 'queued'",
            [id],
        )?;
        Ok(updated > 0)
    }

    /// Marks an op as sent at `sent_at`.
    pub fn mark_pending_op_sent(&self, id: i64, sent_at: OffsetDateTime) -> Result<bool> {
        let updated = self.conn().execute(
            "UPDATE pending_ops SET state = 'sent', sent_at = ?2, last_error = NULL
             WHERE id = ?1",
            rusqlite::params![id, date_to_sql(sent_at)?],
        )?;
        Ok(updated > 0)
    }

    /// Puts an op back in the queue after a failed attempt, to be retried at
    /// `next_retry_at`, counting the attempt if `count_attempt`.
    pub fn requeue_pending_op(
        &self,
        id: i64,
        next_retry_at: OffsetDateTime,
        error: Option<&str>,
        count_attempt: bool,
    ) -> Result<bool> {
        let updated = self.conn().execute(
            "UPDATE pending_ops SET
                 state = 'queued',
                 attempts = attempts + :count,
                 next_retry_at = :next,
                 last_error = coalesce(:error, last_error)
             WHERE id = :id",
            named_params! {
                ":id": id,
                ":count": i64::from(count_attempt),
                ":next": date_to_sql(next_retry_at)?,
                ":error": error,
            },
        )?;
        Ok(updated > 0)
    }

    /// Puts ops left `sending` (by a worker that stopped mid-send) back in the
    /// queue. Returns how many there were.
    pub fn release_sending_ops(&self) -> Result<usize> {
        Ok(self.conn().execute(
            "UPDATE pending_ops SET state = 'queued' WHERE state = 'sending'",
            [],
        )?)
    }

    /// Removes an op from the queue. Returns whether it was there.
    pub fn delete_pending_op(&self, id: i64) -> Result<bool> {
        let deleted = self
            .conn()
            .execute("DELETE FROM pending_ops WHERE id = ?1", [id])?;
        Ok(deleted > 0)
    }

    /// Removes ops sent at or before `up_to`. Returns how many there were.
    pub fn delete_sent_ops(&self, up_to: OffsetDateTime) -> Result<usize> {
        Ok(self.conn().execute(
            "DELETE FROM pending_ops WHERE state = 'sent' AND sent_at <= ?1",
            [date_to_sql(up_to)?],
        )?)
    }

    /// Removes `remove` from a stored message's labels, then adds `add`, and
    /// reports what actually changed. `None`, changing nothing, if the message
    /// is not stored.
    pub fn change_message_labels(
        &self,
        id: &MessageId,
        add: &[LabelId],
        remove: &[LabelId],
    ) -> Result<Option<LabelDelta>> {
        self.transaction(|store| {
            let Some(message) = store.message(id)? else {
                return Ok(None);
            };
            let before: BTreeSet<&LabelId> = message.label_ids.iter().collect();
            let add: BTreeSet<&LabelId> = add.iter().collect();
            let remove: BTreeSet<&LabelId> = remove.iter().filter(|l| !add.contains(l)).collect();
            let delta = LabelDelta {
                added: add
                    .iter()
                    .filter(|l| !before.contains(*l))
                    .map(|l| (*l).clone())
                    .collect(),
                removed: remove
                    .iter()
                    .filter(|l| before.contains(*l))
                    .map(|l| (*l).clone())
                    .collect(),
            };
            store.modify_message_labels(id, &delta.added, &delta.removed)?;
            Ok(Some(delta))
        })
    }

    /// The stored messages carrying `label`.
    pub fn messages_with_label(&self, label: &LabelId) -> Result<Vec<MessageId>> {
        let mut stmt = self.conn().prepare_cached(
            "SELECT message_id FROM message_labels WHERE label_id = ?1 ORDER BY message_id",
        )?;
        let ids = stmt
            .query_map([label.as_str()], |row| {
                row.get::<_, String>(0).map(MessageId::new)
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(ids)
    }

    /// Replaces the label `old` with `new` everywhere: stores `new`, moves
    /// every message from `old` to it, and deletes `old`. For a label created
    /// offline under a temporary ID once Gmail has assigned its real one.
    pub fn replace_label_id(&self, old: &LabelId, new: &Label) -> Result<()> {
        self.transaction(|store| {
            store.upsert_label(new)?;
            // Insert and delete (rather than update) so the triggers keep
            // thread_labels right.
            store.conn().execute(
                "INSERT INTO message_labels (message_id, label_id, thread_id)
                 SELECT message_id, ?2, thread_id FROM message_labels WHERE label_id = ?1
                 ON CONFLICT DO NOTHING",
                [old.as_str(), new.id.as_str()],
            )?;
            store.delete_label(old)?;
            Ok(())
        })
    }
}

/// A `pending_ops` row as stored.
struct OpRow {
    id: i64,
    op_type: String,
    payload: String,
    state: String,
    attempts: i64,
    next_retry_at: i64,
    created_at: i64,
    sent_at: Option<i64>,
    last_error: Option<String>,
}

impl OpRow {
    fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            op_type: row.get(1)?,
            payload: row.get(2)?,
            state: row.get(3)?,
            attempts: row.get(4)?,
            next_retry_at: row.get(5)?,
            created_at: row.get(6)?,
            sent_at: row.get(7)?,
            last_error: row.get(8)?,
        })
    }

    fn into_op(self) -> Result<PendingOp> {
        Ok(PendingOp {
            id: self.id,
            op_type: self.op_type,
            payload: self.payload,
            state: PendingOpState::from_sql(&self.state)?,
            attempts: u32::try_from(u64_from_sql(self.attempts, "op attempts")?)
                .unwrap_or(u32::MAX),
            next_retry_at: date_from_sql(self.next_retry_at)?,
            created_at: date_from_sql(self.created_at)?,
            sent_at: self.sent_at.map(date_from_sql).transpose()?,
            last_error: self.last_error,
        })
    }
}

#[cfg(test)]
mod tests {
    use openmail_core::{
        LabelListVisibility, LabelType, MessageHeaders, MessageListVisibility, Thread, ThreadId,
    };

    use super::*;
    use crate::ThreadView;

    fn date(secs: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(secs).unwrap()
    }

    fn label(id: &str, name: &str) -> Label {
        Label {
            id: LabelId::new(id),
            name: name.to_owned(),
            label_type: LabelType::User,
            color: None,
            label_list_visibility: LabelListVisibility::LabelShow,
            message_list_visibility: MessageListVisibility::Show,
        }
    }

    fn store_with_thread(labels: &[&str]) -> Store {
        let store = Store::open_in_memory().unwrap();
        let thread = Thread {
            id: ThreadId::new("t1"),
            history_id: openmail_core::HistoryId::new(1),
            subject: None,
            snippet: String::new(),
            last_message_date: date(100),
            message_count: 1,
            label_ids: Vec::new(),
            senders: Vec::new(),
            has_attachments: false,
        };
        let message = openmail_core::Message {
            id: MessageId::new("m1"),
            thread_id: thread.id.clone(),
            history_id: openmail_core::HistoryId::new(1),
            label_ids: labels.iter().map(|l| LabelId::new(*l)).collect(),
            snippet: String::new(),
            internal_date: date(100),
            size_estimate: 1,
            headers: MessageHeaders::default(),
            has_attachments: false,
        };
        store.replace_threads(&[(thread, vec![message])]).unwrap();
        store
    }

    fn ids(labels: &[&str]) -> Vec<LabelId> {
        labels.iter().map(|l| LabelId::new(*l)).collect()
    }

    #[test]
    fn ops_queue_claim_requeue_and_send() {
        let store = Store::open_in_memory().unwrap();
        let a = store
            .insert_pending_op("modify", "{}", date(10), date(15))
            .unwrap();
        let b = store
            .insert_pending_op("trash", r#"{"x":1}"#, date(11), date(16))
            .unwrap();
        assert!(b > a);

        let ops = store.pending_ops().unwrap();
        assert_eq!(ops.len(), 2);
        assert_eq!(ops[0].state, PendingOpState::Queued);
        assert_eq!(ops[0].next_retry_at, date(15));
        assert_eq!(ops[1].payload, r#"{"x":1}"#);

        assert!(store.claim_pending_op(a).unwrap());
        assert!(!store.claim_pending_op(a).unwrap(), "already claimed");
        store
            .requeue_pending_op(a, date(20), Some("boom"), true)
            .unwrap();
        let op = store.pending_op(a).unwrap().unwrap();
        assert_eq!(
            (
                op.state,
                op.attempts,
                op.next_retry_at,
                op.last_error.as_deref()
            ),
            (PendingOpState::Queued, 1, date(20), Some("boom"))
        );

        assert!(store.claim_pending_op(a).unwrap());
        assert!(store.claim_pending_op(b).unwrap());
        assert_eq!(store.release_sending_ops().unwrap(), 2);
        assert!(store.claim_pending_op(a).unwrap());
        store.mark_pending_op_sent(a, date(30)).unwrap();
        let op = store.pending_op(a).unwrap().unwrap();
        assert_eq!(
            (op.state, op.sent_at),
            (PendingOpState::Sent, Some(date(30)))
        );

        assert_eq!(store.delete_sent_ops(date(29)).unwrap(), 0);
        assert_eq!(store.delete_sent_ops(date(30)).unwrap(), 1);
        assert!(store.delete_pending_op(b).unwrap());
        assert!(store.pending_ops().unwrap().is_empty());
    }

    #[test]
    fn op_ids_are_never_reused() {
        let store = Store::open_in_memory().unwrap();
        let a = store
            .insert_pending_op("modify", "{}", date(1), date(1))
            .unwrap();
        store.delete_pending_op(a).unwrap();
        let b = store
            .insert_pending_op("modify", "{}", date(1), date(1))
            .unwrap();
        assert!(b > a);
    }

    #[test]
    fn change_message_labels_reports_what_changed() {
        let store = store_with_thread(&["INBOX", "UNREAD"]);
        let m1 = MessageId::new("m1");
        let delta = store
            .change_message_labels(&m1, &ids(&["STARRED", "UNREAD"]), &ids(&["INBOX", "SPAM"]))
            .unwrap()
            .unwrap();
        assert_eq!(delta.added, ids(&["STARRED"]));
        assert_eq!(delta.removed, ids(&["INBOX"]));
        assert_eq!(
            store.message_label_ids(&m1).unwrap(),
            ids(&["STARRED", "UNREAD"])
        );
        let thread = store.thread(&ThreadId::new("t1")).unwrap().unwrap();
        assert!(thread.is_starred() && !thread.has_label(&LabelId::new("INBOX")));

        let unknown = MessageId::new("nope");
        assert_eq!(
            store.change_message_labels(&unknown, &[], &[]).unwrap(),
            None
        );
    }

    #[test]
    fn replace_label_id_moves_messages() {
        let store = store_with_thread(&["INBOX", "pending-1"]);
        store.upsert_label(&label("pending-1", "Work")).unwrap();
        store
            .replace_label_id(&LabelId::new("pending-1"), &label("Label_9", "Work"))
            .unwrap();

        assert_eq!(store.label(&LabelId::new("pending-1")).unwrap(), None);
        assert_eq!(
            store.messages_with_label(&LabelId::new("Label_9")).unwrap(),
            [MessageId::new("m1")]
        );
        let page = store
            .thread_page(&ThreadView::Label(LabelId::new("Label_9")), None, 10)
            .unwrap();
        assert_eq!(page.threads.len(), 1);
        assert!(
            store
                .thread_page(&ThreadView::Label(LabelId::new("pending-1")), None, 10)
                .unwrap()
                .threads
                .is_empty()
        );
    }
}
