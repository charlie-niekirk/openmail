//! Read queries for the UI: paged thread lists.
//!
//! Thread lists page with keysets over `(last_message_date, thread_id)`, newest
//! first, so each page is an index range scan however deep it is.

use openmail_core::{Address, LabelId, SystemLabel, ThreadId};
use rusqlite::Row;
use rusqlite::types::Value;
use time::OffsetDateTime;

use crate::convert::{addresses_from_sql, date_from_sql, date_to_sql};
use crate::{Error, Result, Store};

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
#[cfg(test)]
mod tests;
