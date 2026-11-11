//! The thread list's view-model: which threads of a label are loaded, the
//! queries that load more of them or refresh them, the selection, and how
//! each thread is shown as a row. Plain data with no GPUI types, so it is
//! tested without a window; [`ThreadListView`](crate::ThreadListView) renders
//! it and runs its queries.
//!
//! Threads load a page at a time with [`MailHandle::thread_page`]'s keyset
//! pagination, newest first, as the list scrolls towards its end. Only one
//! query runs at a time. A refresh (after `ThreadsChanged`) re-queries every
//! thread loaded so far in one page, so rows update in place and a thread
//! that gained a message moves to the top.
//!
//! [`MailHandle::thread_page`]: openmail_sync::MailHandle::thread_page

use std::collections::{HashMap, HashSet};

use openmail_core::{
    Address, Label, LabelColor, LabelId, MessageListVisibility, SystemLabel, ThreadId,
    decode_entities,
};
use openmail_store::{ThreadCursor, ThreadPage, ThreadSummary, ThreadView};
use time::OffsetDateTime;

/// Threads per page.
pub const PAGE_SIZE: usize = 50;

/// Load the next page once the last visible row is this close to the end of
/// what is loaded.
pub const LOAD_AHEAD: usize = 25;

/// Label chips shown on a row; the rest are counted.
pub const MAX_CHIPS: usize = 3;

/// What a [`Query`] is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryKind {
    /// The first page of a view, replacing whatever was shown.
    Reset,
    /// Every thread loaded so far, again, to pick up changes.
    Refresh,
    /// The page after the last one loaded.
    More,
}

/// A thread page to fetch for the list. Pass it back to
/// [`ThreadList::finish`] with the result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub kind: QueryKind,
    pub view: ThreadView,
    pub after: Option<ThreadCursor>,
    pub limit: usize,
    /// The view's generation when the query started; results for an older
    /// generation are dropped.
    generation: u64,
}

/// What [`ThreadList::finish`] did with a result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished {
    /// Whether the result changed the list (it was not for an older view).
    pub applied: bool,
    /// A query to run next: a refresh asked for while this one ran.
    pub follow_up: Option<Query>,
}

/// A label chip on a row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelChip {
    pub name: String,
    /// Gmail's colors for the label, if it has some.
    pub color: Option<LabelColor>,
}

/// One thread as the list shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadRow {
    pub id: ThreadId,
    /// The senders, condensed as Gmail does, with the message count.
    pub senders: String,
    pub subject: String,
    pub snippet: String,
    /// The latest message's date, relative to now.
    pub date: String,
    /// Up to [`MAX_CHIPS`] labels to show as chips.
    pub labels: Vec<LabelChip>,
    /// Labels not shown as chips.
    pub more_labels: usize,
    pub unread: bool,
    pub starred: bool,
    pub important: bool,
    pub attachment: bool,
    pub selected: bool,
}

/// The threads of one view, loaded a page at a time.
#[derive(Debug, Clone)]
pub struct ThreadList {
    view: ThreadView,
    threads: Vec<ThreadSummary>,
    /// Where the next page starts; `None` once the last one is loaded.
    next: Option<ThreadCursor>,
    /// Whether the first page has arrived.
    loaded: bool,
    /// Why the last query failed, until one succeeds.
    error: Option<String>,
    in_flight: Option<QueryKind>,
    /// Bumped whenever the view changes, so late results are dropped.
    generation: u64,
    /// A refresh was asked for while a query ran.
    stale: bool,
    selected: Option<ThreadId>,
    labels: HashMap<LabelId, Label>,
    /// The signed-in account's address, shown as "me".
    account: Option<String>,
}

impl Default for ThreadList {
    fn default() -> Self {
        Self::new(inbox())
    }
}

/// The Inbox, which the list shows until told otherwise.
pub fn inbox() -> ThreadView {
    ThreadView::Label(SystemLabel::Inbox.id())
}

impl ThreadList {
    /// An empty list of `view`; [`refresh`](Self::refresh) loads it.
    pub fn new(view: ThreadView) -> Self {
        Self {
            view,
            threads: Vec::new(),
            next: None,
            loaded: false,
            error: None,
            in_flight: None,
            generation: 0,
            stale: false,
            selected: None,
            labels: HashMap::new(),
            account: None,
        }
    }

    pub fn view(&self) -> &ThreadView {
        &self.view
    }

    /// Threads loaded so far.
    pub fn len(&self) -> usize {
        self.threads.len()
    }

    pub fn is_empty(&self) -> bool {
        self.threads.is_empty()
    }

    /// Whether the first page has arrived.
    pub fn is_loaded(&self) -> bool {
        self.loaded
    }

    /// Whether there are threads after the last one loaded.
    pub fn has_more(&self) -> bool {
        self.next.is_some()
    }

    /// Whether the next page is being fetched.
    pub fn is_loading_more(&self) -> bool {
        self.in_flight == Some(QueryKind::More)
    }

    /// Why the last query failed, until one succeeds.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn thread(&self, ix: usize) -> Option<&ThreadSummary> {
        self.threads.get(ix)
    }

    /// Where a thread is in the list, if it is loaded.
    pub fn position(&self, id: &ThreadId) -> Option<usize> {
        self.threads.iter().position(|thread| &thread.id == id)
    }

    pub fn selected(&self) -> Option<&ThreadId> {
        self.selected.as_ref()
    }

    /// Selects a thread. Returns whether the selection changed.
    pub fn select(&mut self, id: ThreadId) -> bool {
        if self.selected.as_ref() == Some(&id) {
            return false;
        }
        self.selected = Some(id);
        true
    }

    /// Sets the labels used for chips.
    pub fn set_labels(&mut self, labels: Vec<Label>) {
        self.labels = labels
            .into_iter()
            .map(|label| (label.id.clone(), label))
            .collect();
    }

    /// Sets the signed-in account, whose address rows show as "me".
    pub fn set_account(&mut self, account: Option<String>) {
        self.account = account;
    }

    /// Shows another view: clears the list and the selection, and returns
    /// the query for its first page. Returns `None` if it is already shown.
    pub fn set_view(&mut self, view: ThreadView) -> Option<Query> {
        if view == self.view {
            return None;
        }
        self.view = view;
        self.threads.clear();
        self.next = None;
        self.loaded = false;
        self.error = None;
        self.stale = false;
        self.selected = None;
        self.generation += 1;
        Some(self.start(QueryKind::Reset))
    }

    /// Loads the first page, or reloads every thread loaded so far. If a
    /// query is running, the refresh runs after it (as
    /// [`Finished::follow_up`]) and this returns `None`.
    pub fn refresh(&mut self) -> Option<Query> {
        if self.in_flight.is_some() {
            self.stale = true;
            return None;
        }
        let kind = if self.loaded {
            QueryKind::Refresh
        } else {
            QueryKind::Reset
        };
        Some(self.start(kind))
    }

    /// The next page, if rows up to `visible_end` (exclusive) are shown and
    /// that is close to the end of what is loaded. Not after a failure, until
    /// a refresh succeeds.
    pub fn load_more(&mut self, visible_end: usize) -> Option<Query> {
        let wanted = self.loaded
            && self.next.is_some()
            && self.in_flight.is_none()
            && self.error.is_none()
            && visible_end + LOAD_AHEAD >= self.threads.len();
        wanted.then(|| self.start(QueryKind::More))
    }

    fn start(&mut self, kind: QueryKind) -> Query {
        self.in_flight = Some(kind);
        let (after, limit) = match kind {
            QueryKind::Reset => (None, PAGE_SIZE),
            QueryKind::Refresh => (None, self.threads.len().max(PAGE_SIZE)),
            QueryKind::More => (self.next.clone(), PAGE_SIZE),
        };
        Query {
            kind,
            view: self.view.clone(),
            after,
            limit,
            generation: self.generation,
        }
    }

    /// Applies a query's result. A result for a view no longer shown is
    /// dropped.
    pub fn finish(&mut self, query: &Query, result: Result<ThreadPage, String>) -> Finished {
        if query.generation != self.generation {
            return Finished {
                applied: false,
                follow_up: None,
            };
        }
        self.in_flight = None;
        match result {
            Ok(page) => {
                self.error = None;
                match query.kind {
                    QueryKind::Reset | QueryKind::Refresh => {
                        self.threads = page.threads;
                        self.loaded = true;
                    }
                    QueryKind::More => {
                        // A thread whose newest message went away can sort
                        // below the cursor and come round again.
                        let known: HashSet<ThreadId> =
                            self.threads.iter().map(|t| t.id.clone()).collect();
                        self.threads.extend(
                            page.threads
                                .into_iter()
                                .filter(|thread| !known.contains(&thread.id)),
                        );
                    }
                }
                self.next = page.next;
            }
            Err(error) => self.error = Some(error),
        }
        let follow_up = if std::mem::take(&mut self.stale) {
            self.refresh()
        } else {
            None
        };
        Finished {
            applied: true,
            follow_up,
        }
    }

    /// Row `ix` as shown at `now`. `local` converts a date to the local time
    /// zone (`now` is already local).
    pub fn row(
        &self,
        ix: usize,
        now: OffsetDateTime,
        local: impl Fn(OffsetDateTime) -> OffsetDateTime,
    ) -> Option<ThreadRow> {
        let thread = self.threads.get(ix)?;
        let mut labels = self.chips(thread);
        let more_labels = labels.len().saturating_sub(MAX_CHIPS);
        labels.truncate(MAX_CHIPS);
        Some(ThreadRow {
            id: thread.id.clone(),
            senders: condense_senders(
                &thread.participants,
                self.account.as_deref(),
                thread.message_count,
            ),
            subject: subject_text(thread.subject.as_deref()),
            snippet: collapse_whitespace(&decode_entities(&thread.snippet)),
            date: format_date(local(thread.last_message_date), now),
            labels,
            more_labels,
            unread: thread.is_unread,
            starred: thread.is_starred,
            important: thread.is_important,
            attachment: thread.has_attachments,
            selected: self.selected.as_ref() == Some(&thread.id),
        })
    }

    /// The thread's labels worth a chip, by name: user labels shown in the
    /// message list, and Inbox outside the Inbox. Not the view's own labels.
    fn chips(&self, thread: &ThreadSummary) -> Vec<LabelChip> {
        let in_view = |id: &LabelId| match &self.view {
            ThreadView::Label(label) => label == id,
            ThreadView::AllOf(labels) => labels.contains(id),
            ThreadView::AllMail => false,
        };
        let mut chips: Vec<LabelChip> = thread
            .label_ids
            .iter()
            .filter(|id| !in_view(id))
            .filter_map(|id| self.labels.get(id))
            .filter(|label| {
                (label.is_user() && label.message_list_visibility == MessageListVisibility::Show)
                    || label.system_label() == Some(SystemLabel::Inbox)
            })
            .map(|label| LabelChip {
                name: label
                    .system_label()
                    .map_or_else(|| label.name.clone(), |s| s.display_name().to_owned()),
                color: label.color.clone(),
            })
            .collect();
        chips.sort_by_key(|chip| chip.name.to_lowercase());
        chips
    }
}

/// The senders as Gmail condenses them: the full name of a lone sender, or
/// first names (the first, then the last two, past three senders), "me" for
/// the account, and the message count if there is more than one.
pub fn condense_senders(participants: &[Address], account: Option<&str>, count: u32) -> String {
    let is_me =
        |address: &Address| account.is_some_and(|me| address.email.eq_ignore_ascii_case(me));
    let names: Vec<&str> = match participants {
        [] => vec!["(no sender)"],
        [only] if is_me(only) => vec!["me"],
        [only] => vec![full_name(only)],
        many => many
            .iter()
            .map(|address| {
                if is_me(address) {
                    "me"
                } else {
                    short_name(address)
                }
            })
            .collect(),
    };
    let mut text = match names.as_slice() {
        [first, .., second_last, last] if names.len() > 3 => {
            format!("{first} .. {second_last}, {last}")
        }
        _ => names.join(", "),
    };
    if count > 1 {
        text.push_str(&format!(" ({count})"));
    }
    text
}

fn full_name(address: &Address) -> &str {
    address
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(&address.email)
}

/// A first name: the first word of the display name ("Ada" for "Lovelace,
/// Ada"), or the address's local part.
fn short_name(address: &Address) -> &str {
    let name = address.name.as_deref().map(str::trim).unwrap_or_default();
    let given = match name.split_once(',') {
        Some((_, given)) if !given.trim().is_empty() => given,
        _ => name,
    };
    if let Some(word) = given
        .split_whitespace()
        .map(|word| word.trim_matches(['"', '\'']))
        .find(|word| !word.is_empty())
    {
        return word;
    }
    address
        .email
        .split_once('@')
        .map_or(&address.email, |(local, _)| local)
}

fn subject_text(subject: Option<&str>) -> String {
    match subject.map(collapse_whitespace) {
        Some(subject) if !subject.is_empty() => subject,
        _ => "(no subject)".to_owned(),
    }
}

fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A date as Gmail's list shows it: the time today ("3:42 PM"), the day
/// this year ("Nov 3"), otherwise the full date ("3/14/24"). Both dates are
/// in local time.
pub fn format_date(date: OffsetDateTime, now: OffsetDateTime) -> String {
    if date.date() == now.date() {
        let (hour, period) = match date.hour() {
            0 => (12, "AM"),
            h @ 1..=11 => (h, "AM"),
            12 => (12, "PM"),
            h => (h - 12, "PM"),
        };
        format!("{hour}:{:02} {period}", date.minute())
    } else if date.year() == now.year() {
        let month = date.month().to_string();
        format!("{} {}", &month[..3], date.day())
    } else {
        format!(
            "{}/{}/{:02}",
            u8::from(date.month()),
            date.day(),
            date.year().rem_euclid(100)
        )
    }
}

#[cfg(test)]
mod tests;
