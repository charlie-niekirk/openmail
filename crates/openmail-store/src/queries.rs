//! Read queries for the UI: paged thread lists, label counts and thread detail.
//!
//! Thread lists page with keysets over `(last_message_date, thread_id)`, newest
//! first, so each page is an index range scan however deep it is.

use openmail_core::{Address, LabelId, Message, SystemLabel, Thread, ThreadId};
use rusqlite::Row;
use rusqlite::types::Value;
use time::OffsetDateTime;

use crate::convert::{addresses_from_sql, date_from_sql, date_to_sql};
use crate::mail::MessageRow;
use crate::{Error, MessageBody, Result, Store};

/// Which threads a thread list shows.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ThreadView {
    /// Threads with this label on any of their messages.
    Label(LabelId),
    /// Threads with every one of these labels, such as `INBOX` and
    /// `CATEGORY_SOCIAL` for an inbox tab. An empty list matches nothing.
    ///
    /// The query walks the first label's threads and checks the others, so it is
    /// fastest when the first label is the most selective.
    AllOf(Vec<LabelId>),
    /// "All Mail": every thread with a message that is not in Spam or Trash.
    AllMail,
}

/// Where the next page of a thread list starts.
///
/// Opaque: it is the sort key of the last thread on a page. Threads that change
/// between pages stay in place relative to it, so paging never repeats or skips a
/// thread whose date did not change. A thread that gains a newer message moves
/// above the cursor and is not shown again on later pages.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ThreadCursor {
    date: i64,
    thread_id: String,
}

/// A page of a thread list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadPage {
    /// The threads, newest first.
    pub threads: Vec<ThreadSummary>,
    /// Where the next page starts, or `None` if this is the last page.
    pub next: Option<ThreadCursor>,
}

/// One row of the thread list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadSummary {
    pub id: ThreadId,
    /// The subject of the thread's first message.
    pub subject: Option<String>,
    /// An excerpt of the latest message.
    pub snippet: String,
    /// The date of the latest message.
    pub last_message_date: OffsetDateTime,
    /// The senders of the thread's messages, oldest first, without duplicates.
    pub participants: Vec<Address>,
    pub message_count: u32,
    pub is_unread: bool,
    pub is_starred: bool,
    pub is_important: bool,
    pub has_attachments: bool,
    /// Every label on any of the thread's messages, sorted by ID.
    pub label_ids: Vec<LabelId>,
}

/// Thread counts for one label, as the sidebar shows them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelCounts {
    pub label_id: LabelId,
    /// Threads with the label.
    pub threads_total: u32,
    /// Threads with the label and an unread message.
    pub threads_unread: u32,
}

/// A thread with its messages, for the conversation view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadDetail {
    pub thread: Thread,
    /// The thread's messages, oldest first.
    pub messages: Vec<MessageDetail>,
}

/// A message in a [`ThreadDetail`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageDetail {
    pub message: Message,
    /// The body, if it has been fetched.
    pub body: Option<MessageBody>,
}

impl Store {
    /// A page of up to `limit` threads in `view`, newest first, starting after
    /// `after` (or at the newest thread). A `limit` of 0 is treated as 1.
    pub fn thread_page(
        &self,
        view: &ThreadView,
        after: Option<&ThreadCursor>,
        limit: usize,
    ) -> Result<ThreadPage> {
        if matches!(view, ThreadView::AllOf(labels) if labels.is_empty()) {
            return Ok(ThreadPage {
                threads: Vec::new(),
                next: None,
            });
        }
        let limit = limit.max(1);
        let (sql, mut params) = thread_page_sql(view, after);
        // One extra row tells whether there is a next page.
        params.push(Value::Integer(
            i64::try_from(limit).unwrap_or(i64::MAX - 1) + 1,
        ));

        let mut stmt = self.conn().prepare_cached(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params), SummaryRow::read)?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let has_more = rows.len() > limit;
        let threads = rows
            .into_iter()
            .take(limit)
            .map(SummaryRow::into_summary)
            .collect::<Result<Vec<_>>>()?;
        let next = threads
            .last()
            .filter(|_| has_more)
            .map(|last| -> Result<ThreadCursor> {
                Ok(ThreadCursor {
                    date: date_to_sql(last.last_message_date)?,
                    thread_id: last.id.as_str().to_owned(),
                })
            })
            .transpose()?;
        Ok(ThreadPage { threads, next })
    }

    /// Total and unread thread counts for every label with at least one thread,
    /// sorted by label ID. Labels missing from the result have no threads.
    pub fn label_counts(&self) -> Result<Vec<LabelCounts>> {
        let mut stmt = self.conn().prepare_cached(LABEL_COUNTS_SQL)?;
        let counts = stmt
            .query_map([], |row| {
                Ok(LabelCounts {
                    label_id: LabelId::new(row.get::<_, String>(0)?),
                    threads_total: row.get(1)?,
                    threads_unread: row.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(counts)
    }

    /// The thread with this ID and its messages, oldest first, with their labels
    /// and any bodies already fetched.
    pub fn thread_detail(&self, id: &ThreadId) -> Result<Option<ThreadDetail>> {
        self.read(|store| {
            let Some(thread) = store.thread(id)? else {
                return Ok(None);
            };
            let mut stmt = store.conn().prepare_cached(THREAD_MESSAGES_SQL)?;
            let rows = stmt
                .query_map([id.as_str()], DetailRow::read)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut messages = rows
                .into_iter()
                .map(DetailRow::into_detail)
                .collect::<Result<Vec<_>>>()?;
            for detail in &mut messages {
                if let Some(body) = &mut detail.body {
                    body.inline = store.inline_parts(&detail.message.id)?;
                }
            }
            Ok(Some(ThreadDetail { thread, messages }))
        })
    }
}

/// The summary columns of the thread aliased `t`, as [`SummaryRow::read`] reads them.
const SUMMARY_COLUMNS: &str = "t.id, t.subject, t.snippet, t.last_message_date, \
     t.message_count, t.senders, t.has_attachments, t.is_unread, t.is_starred, \
     (SELECT json_group_array(l.label_id) \
      FROM thread_labels AS l WHERE l.thread_id = t.id)";

/// The SQL and parameters for a page of `view` after `after`, without the limit,
/// which is the last parameter.
fn thread_page_sql(view: &ThreadView, after: Option<&ThreadCursor>) -> (String, Vec<Value>) {
    let mut params = Vec::new();
    let mut param = |value: Value| {
        params.push(value);
        format!("?{}", params.len())
    };

    let (from, mut filters, key) = match view {
        ThreadView::Label(label) => {
            let label = param(Value::Text(label.as_str().to_owned()));
            label_view(&label, &[])
        }
        ThreadView::AllOf(labels) => {
            let mut labels = labels
                .iter()
                .map(|label| param(Value::Text(label.as_str().to_owned())));
            let first = labels.next().unwrap_or_default();
            let rest: Vec<String> = labels.collect();
            label_view(&first, &rest)
        }
        ThreadView::AllMail => (
            "threads AS t".to_owned(),
            vec![ALL_MAIL_FILTER.to_owned()],
            ("t.last_message_date", "t.id"),
        ),
    };

    if let Some(cursor) = after {
        let date = param(Value::Integer(cursor.date));
        let id = param(Value::Text(cursor.thread_id.clone()));
        filters.push(format!("({}, {}) < ({date}, {id})", key.0, key.1));
    }
    let limit = format!("?{}", params.len() + 1);

    let sql = format!(
        "SELECT {SUMMARY_COLUMNS} FROM {from} WHERE {filters} \
         ORDER BY {date} DESC, {id} DESC LIMIT {limit}",
        filters = filters.join(" AND "),
        date = key.0,
        id = key.1,
    );
    (sql, params)
}

/// The `FROM`, filters and sort key for threads with label `first` and every
/// label in `rest` (all SQL parameters).
fn label_view(first: &str, rest: &[String]) -> (String, Vec<String>, (&'static str, &'static str)) {
    // CROSS JOIN keeps thread_labels as the outer loop, so the page is a range of
    // thread_labels_by_date in index order.
    let from = "thread_labels AS tl CROSS JOIN threads AS t ON t.id = tl.thread_id".to_owned();
    let mut filters = vec![format!("tl.label_id = {first}")];
    filters.extend(rest.iter().map(|label| {
        format!(
            "EXISTS (SELECT 1 FROM thread_labels AS x \
             WHERE x.thread_id = tl.thread_id AND x.label_id = {label})"
        )
    }));
    (from, filters, ("tl.last_message_date", "tl.thread_id"))
}

/// Threads in All Mail: those not in Spam or Trash, or with at least one message
/// in neither.
const ALL_MAIL_FILTER: &str = "(NOT EXISTS (SELECT 1 FROM thread_labels AS x \
       WHERE x.thread_id = t.id AND x.label_id IN ('SPAM', 'TRASH')) \
     OR EXISTS (SELECT 1 FROM messages AS m WHERE m.thread_id = t.id \
       AND NOT EXISTS (SELECT 1 FROM message_labels AS ml \
         WHERE ml.message_id = m.id AND ml.label_id IN ('SPAM', 'TRASH'))))";

/// Total threads per label, and unread ones: a thread is unread when it has the
/// `UNREAD` label, so the unread counts walk only the unread threads' labels.
const LABEL_COUNTS_SQL: &str = "\
    WITH totals AS (
        SELECT label_id, count(*) AS total FROM thread_labels GROUP BY label_id
    ), unread AS (
        SELECT l.label_id, count(*) AS unread
        FROM thread_labels AS u CROSS JOIN thread_labels AS l ON l.thread_id = u.thread_id
        WHERE u.label_id = 'UNREAD'
        GROUP BY l.label_id
    )
    SELECT totals.label_id, totals.total, coalesce(unread.unread, 0)
    FROM totals LEFT JOIN unread ON unread.label_id = totals.label_id
    ORDER BY totals.label_id";

/// A thread's messages oldest first (ties in insertion order), with their labels
/// and bodies. The first 16 columns are [`MessageRow`]'s.
const THREAD_MESSAGES_SQL: &str = "\
    SELECT m.id, m.thread_id, m.history_id, m.snippet, m.internal_date, m.size_estimate,
           m.subject, m.from_addrs, m.to_addrs, m.cc_addrs, m.bcc_addrs, m.reply_to_addrs,
           m.message_id_header, m.in_reply_to, m.\"references\", m.has_attachments,
           (SELECT json_group_array(ml.label_id)
            FROM message_labels AS ml WHERE ml.message_id = m.id),
           b.fetched_at, b.text, b.html
    FROM messages AS m LEFT JOIN bodies AS b ON b.message_id = m.id
    WHERE m.thread_id = ?1
    ORDER BY m.internal_date, m.pk";

/// Label IDs from a JSON array, sorted. (Sorting here rather than in SQL saves a
/// temporary B-tree per row.)
fn label_ids_from_sql(json: &str) -> Result<Vec<LabelId>> {
    let mut ids: Vec<String> =
        serde_json::from_str(json).map_err(|err| Error::Corrupt(format!("label list: {err}")))?;
    ids.sort_unstable();
    Ok(ids.into_iter().map(LabelId::new).collect())
}

/// A thread-list row as selected by [`SUMMARY_COLUMNS`].
struct SummaryRow {
    id: String,
    subject: Option<String>,
    snippet: String,
    last_message_date: i64,
    message_count: u32,
    senders: String,
    has_attachments: bool,
    is_unread: bool,
    is_starred: bool,
    label_ids: String,
}

impl SummaryRow {
    fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            subject: row.get(1)?,
            snippet: row.get(2)?,
            last_message_date: row.get(3)?,
            message_count: row.get(4)?,
            senders: row.get(5)?,
            has_attachments: row.get(6)?,
            is_unread: row.get(7)?,
            is_starred: row.get(8)?,
            label_ids: row.get(9)?,
        })
    }

    fn into_summary(self) -> Result<ThreadSummary> {
        let label_ids = label_ids_from_sql(&self.label_ids)?;
        Ok(ThreadSummary {
            id: ThreadId::new(self.id),
            subject: self.subject,
            snippet: self.snippet,
            last_message_date: date_from_sql(self.last_message_date)?,
            participants: addresses_from_sql(&self.senders)?,
            message_count: self.message_count,
            is_unread: self.is_unread,
            is_starred: self.is_starred,
            is_important: label_ids.iter().any(|id| id.is(SystemLabel::Important)),
            has_attachments: self.has_attachments,
            label_ids,
        })
    }
}

/// A row of [`THREAD_MESSAGES_SQL`].
struct DetailRow {
    message: MessageRow,
    label_ids: String,
    /// Set when the body has been fetched.
    fetched_at: Option<i64>,
    text: Option<String>,
    html: Option<String>,
}

impl DetailRow {
    fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            message: MessageRow::read(row)?,
            label_ids: row.get(16)?,
            fetched_at: row.get(17)?,
            text: row.get(18)?,
            html: row.get(19)?,
        })
    }

    fn into_detail(self) -> Result<MessageDetail> {
        let body = self
            .fetched_at
            .map(|fetched_at| -> Result<MessageBody> {
                Ok(MessageBody {
                    text: self.text,
                    html: self.html,
                    fetched_at: date_from_sql(fetched_at)?,
                    inline: Vec::new(),
                })
            })
            .transpose()?;
        let message = self
            .message
            .into_message(label_ids_from_sql(&self.label_ids)?)?;
        Ok(MessageDetail { message, body })
    }
}

#[cfg(test)]
mod tests;
