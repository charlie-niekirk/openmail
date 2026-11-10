//! Threads, messages, their labels and bodies.

use std::collections::BTreeSet;

use openmail_core::{LabelId, Message, MessageHeaders, MessageId, Thread, ThreadId};
use rusqlite::{OptionalExtension, Row, named_params};
use time::OffsetDateTime;

use crate::convert::{
    addresses_from_sql, addresses_to_sql, date_from_sql, date_to_sql, history_from_sql,
    history_to_sql, u64_from_sql, u64_to_sql,
};
use crate::{Error, Result, Store};

/// A message's body, fetched lazily after its metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageBody {
    /// The plain-text body. Search indexes this, so when a message has only HTML,
    /// store text converted from the HTML here.
    pub text: Option<String>,
    /// The HTML body, as received (sanitizing happens at display time).
    pub html: Option<String>,
    /// When the body was fetched.
    pub fetched_at: OffsetDateTime,
}

impl Store {
    /// Inserts or updates a thread's summary.
    ///
    /// The thread's labels, and so its unread and starred state, come from its
    /// messages' labels: `thread.label_ids` is not stored. Upsert the thread before
    /// its messages.
    pub fn upsert_thread(&self, thread: &Thread) -> Result<()> {
        self.conn().execute(
            "INSERT INTO threads (id, history_id, subject, snippet, last_message_date,
                                  message_count, senders, has_attachments)
             VALUES (:id, :history_id, :subject, :snippet, :date, :count, :senders, :attachments)
             ON CONFLICT (id) DO UPDATE SET
                 history_id = excluded.history_id,
                 subject = excluded.subject,
                 snippet = excluded.snippet,
                 last_message_date = excluded.last_message_date,
                 message_count = excluded.message_count,
                 senders = excluded.senders,
                 has_attachments = excluded.has_attachments",
            named_params! {
                ":id": thread.id.as_str(),
                ":history_id": history_to_sql(thread.history_id)?,
                ":subject": thread.subject,
                ":snippet": thread.snippet,
                ":date": date_to_sql(thread.last_message_date)?,
                ":count": thread.message_count,
                ":senders": addresses_to_sql(&thread.senders)?,
                ":attachments": thread.has_attachments,
            },
        )?;
        Ok(())
    }

    /// The thread with this ID, with the labels of all its messages.
    pub fn thread(&self, id: &ThreadId) -> Result<Option<Thread>> {
        self.read(|store| {
            let row = store
                .conn()
                .query_row(
                    "SELECT id, history_id, subject, snippet, last_message_date,
                            message_count, senders, has_attachments
                     FROM threads WHERE id = ?1",
                    [id.as_str()],
                    ThreadRow::read,
                )
                .optional()?;
            let Some(row) = row else {
                return Ok(None);
            };
            let labels = store.thread_label_ids(id)?;
            row.into_thread(labels).map(Some)
        })
    }

    /// The labels on any of a thread's messages, sorted by ID.
    pub fn thread_label_ids(&self, id: &ThreadId) -> Result<Vec<LabelId>> {
        self.label_ids(
            "SELECT label_id FROM thread_labels WHERE thread_id = ?1 ORDER BY label_id",
            id.as_str(),
        )
    }

    /// Inserts or updates a message's metadata and replaces its labels.
    ///
    /// Its thread must already be stored.
    pub fn upsert_message(&self, message: &Message) -> Result<()> {
        let h = &message.headers;
        self.transaction(|store| {
            store.conn().execute(
                "INSERT INTO messages (id, thread_id, history_id, snippet, internal_date,
                     size_estimate, subject, from_addrs, to_addrs, cc_addrs, bcc_addrs,
                     reply_to_addrs, message_id_header, in_reply_to, \"references\",
                     has_attachments)
                 VALUES (:id, :thread_id, :history_id, :snippet, :date, :size, :subject,
                     :from, :to, :cc, :bcc, :reply_to, :message_id_header, :in_reply_to,
                     :references, :attachments)
                 ON CONFLICT (id) DO UPDATE SET
                     thread_id = excluded.thread_id,
                     history_id = excluded.history_id,
                     snippet = excluded.snippet,
                     internal_date = excluded.internal_date,
                     size_estimate = excluded.size_estimate,
                     subject = excluded.subject,
                     from_addrs = excluded.from_addrs,
                     to_addrs = excluded.to_addrs,
                     cc_addrs = excluded.cc_addrs,
                     bcc_addrs = excluded.bcc_addrs,
                     reply_to_addrs = excluded.reply_to_addrs,
                     message_id_header = excluded.message_id_header,
                     in_reply_to = excluded.in_reply_to,
                     \"references\" = excluded.\"references\",
                     has_attachments = excluded.has_attachments",
                named_params! {
                    ":id": message.id.as_str(),
                    ":thread_id": message.thread_id.as_str(),
                    ":history_id": history_to_sql(message.history_id)?,
                    ":snippet": message.snippet,
                    ":date": date_to_sql(message.internal_date)?,
                    ":size": u64_to_sql(message.size_estimate, "message size")?,
                    ":subject": h.subject,
                    ":from": addresses_to_sql(&h.from)?,
                    ":to": addresses_to_sql(&h.to)?,
                    ":cc": addresses_to_sql(&h.cc)?,
                    ":bcc": addresses_to_sql(&h.bcc)?,
                    ":reply_to": addresses_to_sql(&h.reply_to)?,
                    ":message_id_header": h.message_id_header,
                    ":in_reply_to": h.in_reply_to,
                    ":references": h.references,
                    ":attachments": message.has_attachments,
                },
            )?;
            store.set_message_labels(&message.id, &message.label_ids)
        })
    }

    /// The message with this ID, with its labels sorted by ID.
    pub fn message(&self, id: &MessageId) -> Result<Option<Message>> {
        self.read(|store| {
            let row = store
                .conn()
                .query_row(
                    "SELECT id, thread_id, history_id, snippet, internal_date, size_estimate,
                            subject, from_addrs, to_addrs, cc_addrs, bcc_addrs, reply_to_addrs,
                            message_id_header, in_reply_to, \"references\", has_attachments
                     FROM messages WHERE id = ?1",
                    [id.as_str()],
                    MessageRow::read,
                )
                .optional()?;
            let Some(row) = row else {
                return Ok(None);
            };
            let labels = store.message_label_ids(id)?;
            row.into_message(labels).map(Some)
        })
    }

    /// Deletes a message, with its labels, body and attachments. Returns whether
    /// it existed. Its thread stays, even if it has no messages left.
    pub fn delete_message(&self, id: &MessageId) -> Result<bool> {
        let deleted = self
            .conn()
            .execute("DELETE FROM messages WHERE id = ?1", [id.as_str()])?;
        Ok(deleted > 0)
    }

    /// Replaces a stored message's labels.
    pub fn set_message_labels(&self, id: &MessageId, labels: &[LabelId]) -> Result<()> {
        let wanted: BTreeSet<&str> = labels.iter().map(LabelId::as_str).collect();
        self.transaction(|store| {
            let conn = store.conn();
            let thread_id: String = conn
                .query_row(
                    "SELECT thread_id FROM messages WHERE id = ?1",
                    [id.as_str()],
                    |row| row.get(0),
                )
                .optional()?
                .ok_or_else(|| Error::NotFound(format!("message {id}")))?;

            for current in store.message_label_ids(id)? {
                if !wanted.contains(current.as_str()) {
                    conn.execute(
                        "DELETE FROM message_labels WHERE message_id = ?1 AND label_id = ?2",
                        [id.as_str(), current.as_str()],
                    )?;
                }
            }
            let mut insert = conn.prepare_cached(
                "INSERT INTO message_labels (message_id, label_id, thread_id)
                 VALUES (?1, ?2, ?3) ON CONFLICT DO NOTHING",
            )?;
            for label in wanted {
                insert.execute([id.as_str(), label, &thread_id])?;
            }
            Ok(())
        })
    }

    /// A message's labels, sorted by ID.
    pub fn message_label_ids(&self, id: &MessageId) -> Result<Vec<LabelId>> {
        self.label_ids(
            "SELECT label_id FROM message_labels WHERE message_id = ?1 ORDER BY label_id",
            id.as_str(),
        )
    }

    /// Stores (or replaces) a message's body. The message must already be stored.
    pub fn upsert_body(&self, id: &MessageId, body: &MessageBody) -> Result<()> {
        self.conn().execute(
            "INSERT INTO bodies (message_id, text, html, fetched_at)
             VALUES (:id, :text, :html, :fetched_at)
             ON CONFLICT (message_id) DO UPDATE SET
                 text = excluded.text, html = excluded.html, fetched_at = excluded.fetched_at",
            named_params! {
                ":id": id.as_str(),
                ":text": body.text,
                ":html": body.html,
                ":fetched_at": date_to_sql(body.fetched_at)?,
            },
        )?;
        Ok(())
    }

    /// A message's body, if it has been fetched.
    pub fn body(&self, id: &MessageId) -> Result<Option<MessageBody>> {
        let row: Option<(Option<String>, Option<String>, i64)> = self
            .conn()
            .query_row(
                "SELECT text, html, fetched_at FROM bodies WHERE message_id = ?1",
                [id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        row.map(|(text, html, fetched_at)| {
            Ok(MessageBody {
                text,
                html,
                fetched_at: date_from_sql(fetched_at)?,
            })
        })
        .transpose()
    }

    /// Messages matching an FTS5 query over subject, sender, recipients and body
    /// text, best matches first.
    ///
    /// `query` uses FTS5 syntax, such as `invoice` or `sender:ada AND body:report`;
    /// an invalid query is an error.
    pub fn search_messages(&self, query: &str, limit: usize) -> Result<Vec<MessageId>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut stmt = self.conn().prepare_cached(
            "SELECT m.id FROM messages_fts AS f JOIN messages AS m ON m.pk = f.rowid
             WHERE messages_fts MATCH ?1 ORDER BY f.rank LIMIT ?2",
        )?;
        let ids = stmt
            .query_map(rusqlite::params![query, limit], |row| {
                row.get::<_, String>(0).map(MessageId::new)
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(ids)
    }

    fn label_ids(&self, sql: &str, key: &str) -> Result<Vec<LabelId>> {
        let mut stmt = self.conn().prepare_cached(sql)?;
        let ids = stmt
            .query_map([key], |row| row.get::<_, String>(0).map(LabelId::new))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(ids)
    }
}

/// A `threads` row as stored.
struct ThreadRow {
    id: String,
    history_id: i64,
    subject: Option<String>,
    snippet: String,
    last_message_date: i64,
    message_count: u32,
    senders: String,
    has_attachments: bool,
}

impl ThreadRow {
    fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            history_id: row.get(1)?,
            subject: row.get(2)?,
            snippet: row.get(3)?,
            last_message_date: row.get(4)?,
            message_count: row.get(5)?,
            senders: row.get(6)?,
            has_attachments: row.get(7)?,
        })
    }

    fn into_thread(self, label_ids: Vec<LabelId>) -> Result<Thread> {
        Ok(Thread {
            id: ThreadId::new(self.id),
            history_id: history_from_sql(self.history_id)?,
            subject: self.subject,
            snippet: self.snippet,
            last_message_date: date_from_sql(self.last_message_date)?,
            message_count: self.message_count,
            label_ids,
            senders: addresses_from_sql(&self.senders)?,
            has_attachments: self.has_attachments,
        })
    }
}

/// A `messages` row as stored.
pub(crate) struct MessageRow {
    id: String,
    thread_id: String,
    history_id: i64,
    snippet: String,
    internal_date: i64,
    size_estimate: i64,
    subject: Option<String>,
    from: String,
    to: String,
    cc: String,
    bcc: String,
    reply_to: String,
    message_id_header: Option<String>,
    in_reply_to: Option<String>,
    references: Option<String>,
    has_attachments: bool,
}

impl MessageRow {
    pub(crate) fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            thread_id: row.get(1)?,
            history_id: row.get(2)?,
            snippet: row.get(3)?,
            internal_date: row.get(4)?,
            size_estimate: row.get(5)?,
            subject: row.get(6)?,
            from: row.get(7)?,
            to: row.get(8)?,
            cc: row.get(9)?,
            bcc: row.get(10)?,
            reply_to: row.get(11)?,
            message_id_header: row.get(12)?,
            in_reply_to: row.get(13)?,
            references: row.get(14)?,
            has_attachments: row.get(15)?,
        })
    }

    pub(crate) fn into_message(self, label_ids: Vec<LabelId>) -> Result<Message> {
        Ok(Message {
            id: MessageId::new(self.id),
            thread_id: ThreadId::new(self.thread_id),
            history_id: history_from_sql(self.history_id)?,
            label_ids,
            snippet: self.snippet,
            internal_date: date_from_sql(self.internal_date)?,
            size_estimate: u64_from_sql(self.size_estimate, "message size")?,
            headers: MessageHeaders {
                from: addresses_from_sql(&self.from)?,
                to: addresses_from_sql(&self.to)?,
                cc: addresses_from_sql(&self.cc)?,
                bcc: addresses_from_sql(&self.bcc)?,
                reply_to: addresses_from_sql(&self.reply_to)?,
                subject: self.subject,
                message_id_header: self.message_id_header,
                in_reply_to: self.in_reply_to,
                references: self.references,
            },
            has_attachments: self.has_attachments,
        })
    }
}
