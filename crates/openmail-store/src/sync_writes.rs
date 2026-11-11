//! Bulk writes and lookups for the sync engine.

use std::collections::{BTreeSet, HashMap};

use openmail_core::{HistoryId, Label, LabelId, Message, MessageId, Thread, ThreadId};
use rusqlite::OptionalExtension as _;
use time::OffsetDateTime;

use crate::convert::{date_to_sql, history_from_sql};
use crate::{Result, Store};

impl Store {
    /// Replaces the stored labels with `labels`: upserts each one and deletes
    /// (from every message too) stored labels that are not in the list.
    pub fn replace_labels(&self, labels: &[Label]) -> Result<()> {
        let keep: BTreeSet<&str> = labels.iter().map(|l| l.id.as_str()).collect();
        self.transaction(|store| {
            for stale in store.labels()? {
                if !keep.contains(stale.id.as_str()) {
                    store.delete_label(&stale.id)?;
                }
            }
            labels
                .iter()
                .try_for_each(|label| store.upsert_label(label))
        })
    }

    /// Stores threads, each with exactly the given messages, in one transaction.
    ///
    /// Messages already stored for a thread but missing from its list are
    /// deleted, since they are gone from the server.
    pub fn replace_threads(&self, threads: &[(Thread, Vec<Message>)]) -> Result<()> {
        self.transaction(|store| {
            for (thread, messages) in threads {
                store.upsert_thread(thread)?;
                let keep: BTreeSet<&str> = messages.iter().map(|m| m.id.as_str()).collect();
                for stale in store.thread_message_ids(&thread.id)? {
                    if !keep.contains(stale.as_str()) {
                        store.delete_message(&stale)?;
                    }
                }
                messages
                    .iter()
                    .try_for_each(|message| store.upsert_message(message))?;
            }
            Ok(())
        })
    }

    /// The stored history IDs of those of `ids` that are stored, so sync can skip
    /// threads that have not changed.
    pub fn thread_history_ids(&self, ids: &[ThreadId]) -> Result<HashMap<ThreadId, HistoryId>> {
        self.read(|store| {
            let mut stmt = store
                .conn()
                .prepare_cached("SELECT history_id FROM threads WHERE id = ?1")?;
            let mut found = HashMap::new();
            for id in ids {
                let mut rows = stmt.query([id.as_str()])?;
                if let Some(row) = rows.next()? {
                    found.insert(id.clone(), history_from_sql(row.get(0)?)?);
                }
            }
            Ok(found)
        })
    }

    /// Of the `limit` newest messages carrying `label`, those whose body has not
    /// been fetched, newest first.
    pub fn recent_messages_without_body(
        &self,
        label: &LabelId,
        limit: usize,
    ) -> Result<Vec<MessageId>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut stmt = self.conn().prepare_cached(
            "SELECT id FROM (
                 SELECT m.id, m.internal_date
                 FROM message_labels AS ml JOIN messages AS m ON m.id = ml.message_id
                 WHERE ml.label_id = ?1
                 ORDER BY m.internal_date DESC, m.id DESC
                 LIMIT ?2
             ) AS recent
             WHERE NOT EXISTS (SELECT 1 FROM bodies AS b WHERE b.message_id = recent.id)
             ORDER BY internal_date DESC, id DESC",
        )?;
        let ids = stmt
            .query_map(rusqlite::params![label.as_str(), limit], |row| {
                row.get::<_, String>(0).map(MessageId::new)
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(ids)
    }

    /// Deletes a thread with its messages, their labels and bodies. Returns
    /// whether it was stored.
    pub fn delete_thread(&self, id: &ThreadId) -> Result<bool> {
        let deleted = self
            .conn()
            .execute("DELETE FROM threads WHERE id = ?1", [id.as_str()])?;
        Ok(deleted > 0)
    }

    /// Removes `remove` from a stored message's labels, then adds `add`.
    /// Labels it already has, or lacks, are left alone, so applying the same
    /// change twice is harmless. Returns `false`, changing nothing, if the
    /// message is not stored.
    pub fn modify_message_labels(
        &self,
        id: &MessageId,
        add: &[LabelId],
        remove: &[LabelId],
    ) -> Result<bool> {
        self.transaction(|store| {
            let conn = store.conn();
            let thread_id: Option<String> = conn
                .query_row(
                    "SELECT thread_id FROM messages WHERE id = ?1",
                    [id.as_str()],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(thread_id) = thread_id else {
                return Ok(false);
            };
            let mut delete = conn.prepare_cached(
                "DELETE FROM message_labels WHERE message_id = ?1 AND label_id = ?2",
            )?;
            for label in remove {
                delete.execute([id.as_str(), label.as_str()])?;
            }
            let mut insert = conn.prepare_cached(
                "INSERT INTO message_labels (message_id, label_id, thread_id)
                 VALUES (?1, ?2, ?3) ON CONFLICT DO NOTHING",
            )?;
            for label in add {
                insert.execute([id.as_str(), label.as_str(), &thread_id])?;
            }
            Ok(true)
        })
    }

    /// Records threads listed by the running initial sync, for
    /// [`delete_unseen_threads`](Self::delete_unseen_threads).
    pub fn mark_threads_seen(&self, ids: &[ThreadId]) -> Result<()> {
        self.transaction(|store| {
            let mut insert = store.conn().prepare_cached(
                "INSERT INTO sync_seen_threads (thread_id) VALUES (?1) ON CONFLICT DO NOTHING",
            )?;
            for id in ids {
                insert.execute([id.as_str()])?;
            }
            Ok(())
        })
    }

    /// Forgets which threads were listed.
    pub fn clear_seen_threads(&self) -> Result<()> {
        self.conn().execute("DELETE FROM sync_seen_threads", [])?;
        Ok(())
    }

    /// Deletes stored threads that the finished initial sync did not list,
    /// among those whose last message is at or after `since` (or all of them
    /// with `None`), then forgets the listed threads. Returns how many threads
    /// were deleted.
    ///
    /// Threads older than `since` are outside the sync's range, so they are
    /// kept whether or not they still exist.
    pub fn delete_unseen_threads(&self, since: Option<OffsetDateTime>) -> Result<usize> {
        let since = since.map(date_to_sql).transpose()?.unwrap_or(i64::MIN);
        self.transaction(|store| {
            let deleted = store.conn().execute(
                "DELETE FROM threads
                 WHERE last_message_date >= ?1
                   AND id NOT IN (SELECT thread_id FROM sync_seen_threads)",
                [since],
            )?;
            store.clear_seen_threads()?;
            Ok(deleted)
        })
    }

    /// The IDs of a thread's stored messages.
    pub fn thread_message_ids(&self, id: &ThreadId) -> Result<Vec<MessageId>> {
        let mut stmt = self
            .conn()
            .prepare_cached("SELECT id FROM messages WHERE thread_id = ?1")?;
        let ids = stmt
            .query_map([id.as_str()], |row| {
                row.get::<_, String>(0).map(MessageId::new)
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use openmail_core::{
        Address, LabelListVisibility, LabelType, MessageHeaders, MessageListVisibility,
    };
    use time::OffsetDateTime;

    use super::*;
    use crate::MessageBody;

    fn date(secs: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(secs).unwrap()
    }

    fn label(id: &str) -> Label {
        Label {
            id: LabelId::new(id),
            name: id.to_owned(),
            label_type: LabelType::User,
            color: None,
            label_list_visibility: LabelListVisibility::LabelShow,
            message_list_visibility: MessageListVisibility::Show,
        }
    }

    fn thread(id: &str, history: u64) -> Thread {
        Thread {
            id: ThreadId::new(id),
            history_id: HistoryId::new(history),
            subject: Some("Hello".to_owned()),
            snippet: String::new(),
            last_message_date: date(1_700_000_000),
            message_count: 1,
            label_ids: Vec::new(),
            senders: vec![Address::new("ada@example.com")],
            has_attachments: false,
        }
    }

    fn message(id: &str, thread: &str, secs: i64, labels: &[&str]) -> Message {
        Message {
            id: MessageId::new(id),
            thread_id: ThreadId::new(thread),
            history_id: HistoryId::new(1),
            label_ids: labels.iter().copied().map(LabelId::from).collect(),
            snippet: String::new(),
            internal_date: date(secs),
            size_estimate: 1,
            headers: MessageHeaders::default(),
            has_attachments: false,
        }
    }

    fn ids(ids: &[MessageId]) -> Vec<&str> {
        ids.iter().map(MessageId::as_str).collect()
    }

    #[test]
    fn replace_labels_upserts_and_deletes_stale_labels() {
        let store = Store::open_in_memory().unwrap();
        store.replace_labels(&[label("A"), label("B")]).unwrap();
        store
            .replace_threads(&[(thread("t", 1), vec![message("m", "t", 1, &["A", "B"])])])
            .unwrap();

        let mut renamed = label("A");
        renamed.name = "Renamed".to_owned();
        store.replace_labels(&[renamed, label("C")]).unwrap();

        let names: Vec<_> = store
            .labels()
            .unwrap()
            .into_iter()
            .map(|l| l.name)
            .collect();
        assert_eq!(names, ["C", "Renamed"]);
        let labels = store.message_label_ids(&MessageId::new("m")).unwrap();
        assert_eq!(labels, [LabelId::new("A")]);
    }

    #[test]
    fn replace_threads_drops_messages_gone_from_the_thread() {
        let store = Store::open_in_memory().unwrap();
        store
            .replace_threads(&[(
                thread("t", 1),
                vec![
                    message("m1", "t", 1, &["INBOX"]),
                    message("m2", "t", 2, &[]),
                ],
            )])
            .unwrap();
        store
            .replace_threads(&[(
                thread("t", 2),
                vec![
                    message("m2", "t", 2, &["INBOX"]),
                    message("m3", "t", 3, &[]),
                ],
            )])
            .unwrap();

        assert!(store.message(&MessageId::new("m1")).unwrap().is_none());
        assert!(store.message(&MessageId::new("m3")).unwrap().is_some());
        let t = store.thread(&ThreadId::new("t")).unwrap().unwrap();
        assert_eq!(t.history_id, HistoryId::new(2));
        assert_eq!(t.label_ids, [LabelId::new("INBOX")]);
    }

    #[test]
    fn thread_history_ids_reports_only_stored_threads() {
        let store = Store::open_in_memory().unwrap();
        store
            .replace_threads(&[(thread("t", 7), vec![message("m", "t", 1, &[])])])
            .unwrap();
        let found = store
            .thread_history_ids(&[ThreadId::new("t"), ThreadId::new("missing")])
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[&ThreadId::new("t")], HistoryId::new(7));
    }

    #[test]
    fn delete_thread_removes_its_messages_and_labels() {
        let store = Store::open_in_memory().unwrap();
        store
            .replace_threads(&[(thread("t", 1), vec![message("m", "t", 1, &["INBOX"])])])
            .unwrap();
        assert!(store.delete_thread(&ThreadId::new("t")).unwrap());
        assert!(!store.delete_thread(&ThreadId::new("t")).unwrap());
        assert!(store.message(&MessageId::new("m")).unwrap().is_none());
        let counts = store.label_counts().unwrap();
        assert!(counts.iter().all(|c| c.threads_total == 0), "{counts:?}");
    }

    #[test]
    fn modify_message_labels_is_idempotent_and_updates_thread_flags() {
        let store = Store::open_in_memory().unwrap();
        store
            .replace_threads(&[(thread("t", 1), vec![message("m", "t", 1, &["INBOX"])])])
            .unwrap();
        let m = MessageId::new("m");
        let add = [LabelId::new("UNREAD"), LabelId::new("STARRED")];
        for _ in 0..2 {
            assert!(store.modify_message_labels(&m, &add, &[]).unwrap());
        }
        let t = store.thread(&ThreadId::new("t")).unwrap().unwrap();
        assert_eq!(t.label_ids.len(), 3);
        let summary = &store
            .thread_page(&crate::ThreadView::Label(LabelId::new("INBOX")), None, 10)
            .unwrap()
            .threads[0];
        assert!(summary.is_unread && summary.is_starred);

        let remove = [LabelId::new("INBOX"), LabelId::new("UNREAD")];
        for _ in 0..2 {
            assert!(store.modify_message_labels(&m, &[], &remove).unwrap());
        }
        assert_eq!(
            store.message_label_ids(&m).unwrap(),
            [LabelId::new("STARRED")]
        );
        let summary = &store
            .thread_page(&crate::ThreadView::Label(LabelId::new("STARRED")), None, 10)
            .unwrap()
            .threads[0];
        assert!(!summary.is_unread && summary.is_starred);

        assert!(
            !store
                .modify_message_labels(&MessageId::new("missing"), &add, &[])
                .unwrap()
        );
    }

    #[test]
    fn delete_unseen_threads_keeps_listed_and_older_threads() {
        let store = Store::open_in_memory().unwrap();
        let mut old = thread("old", 1);
        old.last_message_date = date(100);
        let mut gone = thread("gone", 1);
        gone.last_message_date = date(300);
        let mut listed = thread("listed", 1);
        listed.last_message_date = date(300);
        store
            .replace_threads(&[
                (old, vec![message("m1", "old", 100, &[])]),
                (gone, vec![message("m2", "gone", 300, &[])]),
                (listed, vec![message("m3", "listed", 300, &[])]),
            ])
            .unwrap();
        store.mark_threads_seen(&[ThreadId::new("listed")]).unwrap();
        store.mark_threads_seen(&[ThreadId::new("listed")]).unwrap();

        assert_eq!(store.delete_unseen_threads(Some(date(200))).unwrap(), 1);
        assert!(store.thread(&ThreadId::new("gone")).unwrap().is_none());
        assert!(store.thread(&ThreadId::new("old")).unwrap().is_some());
        assert!(store.thread(&ThreadId::new("listed")).unwrap().is_some());

        // The listed set was cleared, so with no cutoff everything goes.
        assert_eq!(store.delete_unseen_threads(None).unwrap(), 2);
    }

    #[test]
    fn recent_messages_without_body_skips_fetched_bodies() {
        let store = Store::open_in_memory().unwrap();
        store
            .replace_threads(&[
                (
                    thread("t1", 1),
                    vec![
                        message("old", "t1", 1, &["INBOX"]),
                        message("archived", "t1", 5, &[]),
                    ],
                ),
                (
                    thread("t2", 1),
                    vec![
                        message("mid", "t2", 2, &["INBOX"]),
                        message("new", "t2", 3, &["INBOX"]),
                    ],
                ),
            ])
            .unwrap();
        let inbox = LabelId::new("INBOX");

        let recent = store.recent_messages_without_body(&inbox, 2).unwrap();
        assert_eq!(ids(&recent), ["new", "mid"]);

        let body = MessageBody {
            text: Some("hi".to_owned()),
            html: None,
            fetched_at: date(10),
            inline: Vec::new(),
        };
        store.upsert_body(&MessageId::new("new"), &body).unwrap();
        // Only the two newest are considered, so "old" is not pulled in.
        let recent = store.recent_messages_without_body(&inbox, 2).unwrap();
        assert_eq!(ids(&recent), ["mid"]);
        let recent = store.recent_messages_without_body(&inbox, 10).unwrap();
        assert_eq!(ids(&recent), ["mid", "old"]);
    }
}
