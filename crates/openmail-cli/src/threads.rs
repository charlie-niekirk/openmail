//! `threads <label>`: the newest threads in a label.

use anyhow::Context as _;
use futures_util::{StreamExt as _, TryStreamExt as _, stream};
use openmail_core::{Label, LabelId, Thread, ThreadId, decode_entities};
use openmail_gmail::GmailClient;
use openmail_gmail::api::{self, ApiLabel, ListRequest, ThreadFormat};
use time::UtcOffset;

use crate::text::{collapse_whitespace, format_date, truncate};

/// How many thread requests run at once.
const CONCURRENCY: usize = 8;
/// The largest page `threads.list` returns.
const MAX_PAGE: u32 = 500;
/// The headers a thread list needs.
const HEADERS: &[&str] = &["From", "Subject", "Date"];

/// Width of the sender column, including the message count.
const SENDER_WIDTH: usize = 24;
/// Width of the subject and snippet column.
const SUMMARY_WIDTH: usize = 80;

/// Prints up to `limit` threads in `label`, optionally narrowed by a Gmail
/// search `query`.
pub async fn run(
    client: &GmailClient,
    label: &str,
    limit: u32,
    query: Option<String>,
    offset: UtcOffset,
) -> anyhow::Result<()> {
    let labels = client
        .list_labels()
        .await
        .context("could not list labels")?;
    let label_id = resolve_label(&labels, label).with_context(|| {
        format!("no label called {label:?}; run `openmail-cli labels` to list them")
    })?;

    let mut request = ListRequest::new()
        .label(label_id)
        .max_results(limit.min(MAX_PAGE));
    if let Some(query) = query {
        request = request.query(query);
    }
    let ids: Vec<ThreadId> = client
        .thread_pages(request)
        .map_ok(|page| stream::iter(page.threads.into_iter().map(|t| Ok::<_, api::Error>(t.id))))
        .try_flatten()
        .take(usize::try_from(limit).unwrap_or(usize::MAX))
        .try_collect()
        .await
        .context("could not list threads")?;

    let threads: Vec<Thread> = stream::iter(ids)
        .map(|id| {
            let client = client.clone();
            async move {
                let thread = client
                    .get_thread(&id, ThreadFormat::Metadata, HEADERS)
                    .await?;
                thread.into_domain().map(|(thread, _)| thread)
            }
        })
        .buffered(CONCURRENCY)
        .try_collect()
        .await
        .context("could not fetch threads")?;

    if threads.is_empty() {
        println!("No threads.");
    } else {
        print!("{}", format_threads(&threads, offset));
    }
    Ok(())
}

/// Finds a label by ID, then by name, then by the name Gmail web shows for a
/// system label (such as `Drafts` for `DRAFT`), ignoring case after the exact ID.
pub fn resolve_label(labels: &[ApiLabel], wanted: &str) -> Option<LabelId> {
    let by = |matches: &dyn Fn(&ApiLabel) -> bool| labels.iter().find(|l| matches(l));
    by(&|l| l.id.as_str() == wanted)
        .or_else(|| by(&|l| l.id.as_str().eq_ignore_ascii_case(wanted)))
        .or_else(|| by(&|l| l.name.eq_ignore_ascii_case(wanted)))
        .or_else(|| {
            by(&|l| {
                Label::from(l.clone())
                    .system_label()
                    .is_some_and(|s| s.display_name().eq_ignore_ascii_case(wanted))
            })
        })
        .map(|l| l.id.clone())
}

/// One line per thread: ID, date of the newest message, an unread marker, the
/// senders with the message count, then the subject and snippet.
pub fn format_threads(threads: &[Thread], offset: UtcOffset) -> String {
    let id_width = threads
        .iter()
        .map(|t| t.id.as_str().len())
        .max()
        .unwrap_or_default();
    let mut out = String::new();
    for thread in threads {
        let unread = if thread.is_unread() { '*' } else { ' ' };
        let line = format!(
            "{id:<id_width$}  {date}  {unread} {sender:<SENDER_WIDTH$}  {summary}",
            id = thread.id.as_str(),
            date = format_date(thread.last_message_date, offset),
            sender = sender_column(thread),
            summary = summary_column(thread),
        );
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// The senders' names (or addresses), then `(n)` if the thread has several
/// messages, fitted to [`SENDER_WIDTH`].
fn sender_column(thread: &Thread) -> String {
    let names = thread
        .senders
        .iter()
        .map(|a| collapse_whitespace(a.display_name_or_email()))
        .collect::<Vec<_>>()
        .join(", ");
    let names = if names.is_empty() {
        "(no sender)".to_owned()
    } else {
        names
    };
    let count = if thread.message_count > 1 {
        format!(" ({})", thread.message_count)
    } else {
        String::new()
    };
    let room = SENDER_WIDTH.saturating_sub(count.chars().count());
    format!("{}{count}", truncate(&names, room))
}

/// `Subject — snippet`, fitted to [`SUMMARY_WIDTH`].
fn summary_column(thread: &Thread) -> String {
    let subject = thread
        .subject
        .as_deref()
        .map(collapse_whitespace)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(no subject)".to_owned());
    let snippet = collapse_whitespace(&decode_entities(&thread.snippet));
    let summary = if snippet.is_empty() {
        subject
    } else {
        format!("{subject} — {snippet}")
    };
    truncate(&summary, SUMMARY_WIDTH)
}

#[cfg(test)]
mod tests {
    use openmail_core::{Address, HistoryId, LabelType, SystemLabel};
    use time::macros::{datetime, offset};

    use super::*;

    fn thread(id: &str, senders: Vec<Address>, count: u32, unread: bool) -> Thread {
        let mut label_ids = vec![SystemLabel::Inbox.id()];
        if unread {
            label_ids.push(SystemLabel::Unread.id());
        }
        Thread {
            id: ThreadId::new(id),
            history_id: HistoryId::new(1),
            subject: Some("Launch plan".to_owned()),
            snippet: "Here&#39;s the latest draft".to_owned(),
            last_message_date: datetime!(2026-11-10 14:03 UTC),
            message_count: count,
            label_ids,
            senders,
            has_attachments: false,
        }
    }

    #[test]
    fn formats_one_line_per_thread() {
        let threads = [
            thread(
                "18c2f0a1b2c3d4e5",
                vec![
                    Address::named("Ada Lovelace", "ada@example.com"),
                    Address::new("grace@example.com"),
                ],
                3,
                true,
            ),
            thread(
                "18c2f0a1b2c3d4e6",
                vec![Address::new("bob@example.com")],
                1,
                false,
            ),
        ];
        assert_eq!(
            format_threads(&threads, offset!(+1)),
            "\
18c2f0a1b2c3d4e5  2026-11-10 15:03  * Ada Lovelace, grace… (3)  Launch plan — Here's the latest draft
18c2f0a1b2c3d4e6  2026-11-10 15:03    bob@example.com           Launch plan — Here's the latest draft
"
        );
    }

    #[test]
    fn long_summaries_are_truncated() {
        let mut t = thread("1", vec![Address::new("a@example.com")], 1, false);
        t.subject = Some("Re:\r\n  quarterly   numbers".to_owned());
        t.snippet = "x".repeat(200);
        let summary = summary_column(&t);
        assert!(summary.starts_with("Re: quarterly numbers — xxx"));
        assert!(summary.ends_with('…'));
        assert_eq!(summary.chars().count(), SUMMARY_WIDTH);
    }

    #[test]
    fn missing_subject_snippet_and_sender() {
        let mut t = thread("1", Vec::new(), 1, false);
        t.subject = None;
        t.snippet = String::new();
        assert_eq!(summary_column(&t), "(no subject)");
        assert_eq!(sender_column(&t), "(no sender)");
    }

    fn api_label(id: &str, name: &str, label_type: LabelType) -> ApiLabel {
        ApiLabel {
            id: LabelId::new(id),
            name: name.to_owned(),
            label_type: Some(label_type),
            message_list_visibility: None,
            label_list_visibility: None,
            color: None,
            messages_total: None,
            messages_unread: None,
            threads_total: None,
            threads_unread: None,
        }
    }

    #[test]
    fn resolves_labels_by_id_name_or_display_name() {
        let labels = [
            api_label("INBOX", "INBOX", LabelType::System),
            api_label("DRAFT", "DRAFT", LabelType::System),
            api_label("CATEGORY_SOCIAL", "CATEGORY_SOCIAL", LabelType::System),
            api_label("Label_1", "Work/Projects", LabelType::User),
            api_label("Label_2", "inbox", LabelType::User),
        ];
        let resolve = |wanted| resolve_label(&labels, wanted).map(LabelId::into_string);
        assert_eq!(resolve("INBOX").as_deref(), Some("INBOX"));
        assert_eq!(resolve("Label_1").as_deref(), Some("Label_1"));
        assert_eq!(resolve("work/projects").as_deref(), Some("Label_1"));
        assert_eq!(resolve("Drafts").as_deref(), Some("DRAFT"));
        assert_eq!(resolve("social").as_deref(), Some("CATEGORY_SOCIAL"));
        // An ID, in any case, wins over a user label's name.
        assert_eq!(resolve("inbox").as_deref(), Some("INBOX"));
        assert_eq!(resolve("Nope"), None);
    }
}
