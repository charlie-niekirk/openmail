//! Thread list tests: paging and refreshing against a real (in-memory)
//! store, and row formatting.

use openmail_core::{
    Address, HistoryId, Label, LabelColor, LabelId, LabelListVisibility, LabelType, Message,
    MessageHeaders, MessageId, MessageListVisibility, Thread, ThreadId,
};
use openmail_store::{Store, ThreadPage, ThreadView};
use time::OffsetDateTime;
use time::macros::{datetime, offset};

use super::*;

fn date(secs: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(secs).unwrap()
}

fn view(id: &str) -> ThreadView {
    ThreadView::Label(LabelId::new(id))
}

/// Stores thread `id` with one message, dated `secs`, with `labels`.
fn put(store: &Store, id: &str, secs: i64, labels: &[&str]) {
    let sender = Address::named("Ada Lovelace", "ada@example.com");
    store
        .upsert_thread(&Thread {
            id: ThreadId::new(id),
            history_id: HistoryId::new(1),
            subject: Some(format!("Subject {id}")),
            snippet: format!("Snippet {id}"),
            last_message_date: date(secs),
            message_count: 1,
            label_ids: Vec::new(),
            senders: vec![sender.clone()],
            has_attachments: false,
        })
        .unwrap();
    store
        .upsert_message(&Message {
            id: MessageId::new(format!("{id}-m")),
            thread_id: ThreadId::new(id),
            history_id: HistoryId::new(1),
            label_ids: labels.iter().copied().map(LabelId::from).collect(),
            snippet: format!("Snippet {id}"),
            internal_date: date(secs),
            size_estimate: 100,
            headers: MessageHeaders {
                from: vec![sender],
                subject: Some(format!("Subject {id}")),
                ..MessageHeaders::default()
            },
            has_attachments: false,
        })
        .unwrap();
}

/// A store with `count` Inbox threads, `t0` (oldest) to `t{count-1}`
/// (newest).
fn inbox_store(count: i64) -> Store {
    let store = Store::open_in_memory().unwrap();
    for n in 0..count {
        put(&store, &format!("t{n}"), 1_000 + n, &["INBOX"]);
    }
    store
}

fn run(store: &Store, query: &Query) -> Result<ThreadPage, String> {
    store
        .thread_page(&query.view, query.after.as_ref(), query.limit)
        .map_err(|err| err.to_string())
}

/// Runs `query` and applies its result, then any follow-ups.
fn complete(list: &mut ThreadList, store: &Store, query: Query) {
    let mut next = Some(query);
    while let Some(query) = next {
        let finished = list.finish(&query, run(store, &query));
        assert!(finished.applied);
        next = finished.follow_up;
    }
}

fn ids(list: &ThreadList) -> Vec<String> {
    (0..list.len())
        .map(|ix| list.thread(ix).unwrap().id.as_str().to_owned())
        .collect()
}

fn newest_first(range: std::ops::Range<i64>) -> Vec<String> {
    range.rev().map(|n| format!("t{n}")).collect()
}

#[test]
fn starts_on_the_inbox() {
    let list = ThreadList::default();
    assert_eq!(list.view(), &inbox());
    assert!(!list.is_loaded());
    assert!(list.is_empty());
}

#[test]
fn pages_in_as_the_list_scrolls() {
    let store = inbox_store(120);
    let mut list = ThreadList::default();

    let first = list.refresh().unwrap();
    assert_eq!(first.kind, QueryKind::Reset);
    assert_eq!(first.limit, PAGE_SIZE);
    assert_eq!(list.load_more(PAGE_SIZE), None, "not before the first page");
    complete(&mut list, &store, first);
    assert!(list.is_loaded());
    assert_eq!(ids(&list), newest_first(70..120));
    assert!(list.has_more());

    // Far from the end: nothing to load yet.
    assert_eq!(list.load_more(PAGE_SIZE - LOAD_AHEAD - 1), None);
    let more = list.load_more(PAGE_SIZE - LOAD_AHEAD).unwrap();
    assert_eq!(more.kind, QueryKind::More);
    assert!(list.is_loading_more());
    assert_eq!(list.load_more(PAGE_SIZE), None, "one query at a time");
    complete(&mut list, &store, more);
    assert_eq!(list.len(), 100);

    let more = list.load_more(100).unwrap();
    complete(&mut list, &store, more);
    assert_eq!(ids(&list), newest_first(0..120));
    assert!(!list.has_more());
    assert_eq!(list.load_more(120), None, "the last page is loaded");
}

#[test]
fn refresh_updates_loaded_rows_in_place_and_puts_new_threads_on_top() {
    let store = inbox_store(120);
    let mut list = ThreadList::default();
    let first = list.refresh().unwrap();
    complete(&mut list, &store, first);
    let more = list.load_more(PAGE_SIZE).unwrap();
    complete(&mut list, &store, more);
    assert_eq!(list.len(), 100);

    // The row at the top of the viewport, which the view keeps in place.
    let anchor = list.thread(3).unwrap().id.clone();
    assert_eq!(anchor.as_str(), "t116");

    // A new thread arrives, t116 is read, and t118 leaves the Inbox.
    put(&store, "new", 5_000, &["INBOX", "UNREAD"]);
    store
        .set_message_labels(&MessageId::new("t118-m"), &[LabelId::new("ARCHIVE")])
        .unwrap();
    put(&store, "t116", 1_116, &["INBOX", "STARRED"]);

    let refresh = list.refresh().unwrap();
    assert_eq!(refresh.kind, QueryKind::Refresh);
    assert_eq!(refresh.after, None);
    assert_eq!(refresh.limit, 100, "everything loaded, in one query");
    complete(&mut list, &store, refresh);

    assert_eq!(list.len(), 100);
    assert_eq!(list.thread(0).unwrap().id.as_str(), "new");
    assert!(list.thread(0).unwrap().is_unread);
    assert_eq!(list.position(&ThreadId::new("t118")), None);
    // One thread above it arrived and one left: the anchor stays at 3.
    assert_eq!(list.position(&anchor), Some(3));
    assert!(list.thread(3).unwrap().is_starred);

    // Paging continues after the refreshed rows without repeats.
    let more = list.load_more(100).unwrap();
    complete(&mut list, &store, more);
    let mut expected = vec!["new".to_owned()];
    expected.extend(newest_first(0..120).into_iter().filter(|id| id != "t118"));
    assert_eq!(ids(&list), expected);
}

#[test]
fn a_refresh_asked_for_during_a_query_follows_it() {
    let store = inbox_store(10);
    let mut list = ThreadList::default();
    let first = list.refresh().unwrap();
    assert_eq!(list.refresh(), None, "queued behind the first page");

    put(&store, "new", 5_000, &["INBOX"]);
    let finished = list.finish(&first, run(&store, &first));
    let follow_up = finished.follow_up.expect("a refresh follows");
    assert_eq!(follow_up.kind, QueryKind::Refresh);
    complete(&mut list, &store, follow_up);
    assert_eq!(list.thread(0).unwrap().id.as_str(), "new");
    assert_eq!(list.len(), 11);
}

#[test]
fn switching_views_drops_late_results() {
    let store = inbox_store(80);
    put(&store, "work", 2_000, &["Label_1"]);
    let mut list = ThreadList::default();
    let first = list.refresh().unwrap();
    complete(&mut list, &store, first);
    list.select(ThreadId::new("t79"));
    let more = list.load_more(PAGE_SIZE).unwrap();

    assert_eq!(list.set_view(inbox()), None, "already shown");
    let reset = list.set_view(view("Label_1")).unwrap();
    assert_eq!(reset.kind, QueryKind::Reset);
    assert!(list.is_empty());
    assert_eq!(list.selected(), None);

    // The Inbox page arrives late and is dropped.
    let late = list.finish(&more, run(&store, &more));
    assert!(!late.applied);
    assert!(list.is_empty());

    complete(&mut list, &store, reset);
    assert_eq!(ids(&list), ["work"]);
    assert!(!list.has_more());
}

#[test]
fn failures_are_reported_and_retried_by_a_refresh() {
    let store = inbox_store(80);
    let mut list = ThreadList::default();

    let first = list.refresh().unwrap();
    list.finish(&first, Err("store failed".to_owned()));
    assert_eq!(list.error(), Some("store failed"));
    assert!(!list.is_loaded());

    let retry = list.refresh().unwrap();
    assert_eq!(retry.kind, QueryKind::Reset);
    complete(&mut list, &store, retry);
    assert_eq!(list.error(), None);

    // A failed page is not retried by scrolling, only by a refresh.
    let more = list.load_more(PAGE_SIZE).unwrap();
    list.finish(&more, Err("busy".to_owned()));
    assert_eq!(list.len(), PAGE_SIZE, "rows are kept");
    assert_eq!(list.load_more(PAGE_SIZE), None);
    let refresh = list.refresh().unwrap();
    complete(&mut list, &store, refresh);
    assert!(list.load_more(PAGE_SIZE).is_some());
}

#[test]
fn selection_survives_refreshes() {
    let store = inbox_store(3);
    let mut list = ThreadList::default();
    let first = list.refresh().unwrap();
    complete(&mut list, &store, first);

    assert!(list.select(ThreadId::new("t1")));
    assert!(!list.select(ThreadId::new("t1")), "unchanged");
    let refresh = list.refresh().unwrap();
    complete(&mut list, &store, refresh);
    assert_eq!(list.selected().map(ThreadId::as_str), Some("t1"));

    let now = datetime!(2026-11-11 12:00 UTC);
    let rows: Vec<bool> = (0..3)
        .map(|ix| list.row(ix, now, |d| d).unwrap().selected)
        .collect();
    assert_eq!(rows, [false, true, false]);
}

fn ada() -> Address {
    Address::named("Ada Lovelace", "ada@example.com")
}

#[test]
fn a_lone_sender_shows_in_full() {
    assert_eq!(condense_senders(&[ada()], None, 1), "Ada Lovelace");
    assert_eq!(condense_senders(&[ada()], None, 4), "Ada Lovelace (4)");
    assert_eq!(
        condense_senders(&[Address::new("bob@example.com")], None, 1),
        "bob@example.com"
    );
    assert_eq!(
        condense_senders(&[Address::named("  ", "bob@example.com")], None, 1),
        "bob@example.com"
    );
    assert_eq!(condense_senders(&[], None, 1), "(no sender)");
}

#[test]
fn several_senders_show_first_names_and_me() {
    let me = Some("Me@Example.com");
    let senders = [
        ada(),
        Address::new("me@example.com"),
        Address::named("Hopper, Grace", "grace@example.com"),
    ];
    assert_eq!(condense_senders(&senders, me, 5), "Ada, me, Grace (5)");
    assert_eq!(
        condense_senders(&[Address::new("me@example.com")], me, 2),
        "me (2)"
    );
    assert_eq!(
        condense_senders(
            &[
                Address::new("bob.smith@example.com"),
                Address::named("\"Alan\" Turing", "alan@example.com"),
            ],
            None,
            2,
        ),
        "bob.smith, Alan (2)"
    );
}

#[test]
fn many_senders_show_the_first_and_the_last_two() {
    let senders: Vec<Address> = ["Ada", "Bob", "Cy", "Dee", "Eve"]
        .iter()
        .map(|name| Address::named(*name, format!("{name}@example.com")))
        .collect();
    assert_eq!(condense_senders(&senders, None, 7), "Ada .. Dee, Eve (7)");
    assert_eq!(condense_senders(&senders[..3], None, 3), "Ada, Bob, Cy (3)");
}

#[test]
fn dates_are_relative_to_now() {
    let now = datetime!(2026-11-11 16:20 +1);
    let at = |date| format_date(date, now);
    assert_eq!(at(datetime!(2026-11-11 15:42 +1)), "3:42 PM");
    assert_eq!(at(datetime!(2026-11-11 00:05 +1)), "12:05 AM");
    assert_eq!(at(datetime!(2026-11-11 12:00 +1)), "12:00 PM");
    assert_eq!(at(datetime!(2026-11-11 9:07 +1)), "9:07 AM");
    assert_eq!(at(datetime!(2026-11-10 23:59 +1)), "Nov 10");
    assert_eq!(at(datetime!(2026-01-03 08:00 +1)), "Jan 3");
    assert_eq!(at(datetime!(2025-12-31 08:00 +1)), "12/31/25");
    assert_eq!(at(datetime!(2009-03-04 08:00 +1)), "3/4/09");
}

#[test]
fn rows_use_local_time() {
    let store = Store::open_in_memory().unwrap();
    // 23:30 UTC on the 10th is 00:30 on the 11th at +1.
    let utc = datetime!(2026-11-10 23:30 UTC);
    put(&store, "t", utc.unix_timestamp(), &["INBOX"]);
    let mut list = ThreadList::default();
    let first = list.refresh().unwrap();
    complete(&mut list, &store, first);

    let now = datetime!(2026-11-11 09:00 +1);
    let row = list.row(0, now, |d| d.to_offset(offset!(+1))).unwrap();
    assert_eq!(row.date, "12:30 AM");
    let row = list.row(0, now, |d| d).unwrap();
    assert_eq!(row.date, "Nov 10");
}

fn label(id: &str, name: &str, kind: LabelType) -> Label {
    Label {
        id: LabelId::new(id),
        name: name.to_owned(),
        label_type: kind,
        color: None,
        label_list_visibility: LabelListVisibility::LabelShow,
        message_list_visibility: MessageListVisibility::Show,
    }
}

fn labels() -> Vec<Label> {
    let mut hidden = label("Label_hidden", "Hidden", LabelType::User);
    hidden.message_list_visibility = MessageListVisibility::Hide;
    let mut work = label("Label_work", "Work", LabelType::User);
    work.color = Some(LabelColor {
        text_color: "#ffffff".to_owned(),
        background_color: "#16a765".to_owned(),
    });
    vec![
        label("INBOX", "INBOX", LabelType::System),
        label("STARRED", "STARRED", LabelType::System),
        label("CATEGORY_UPDATES", "CATEGORY_UPDATES", LabelType::System),
        hidden,
        work,
        label("Label_a", "accounts", LabelType::User),
        label("Label_b", "Bills", LabelType::User),
        label("Label_c", "Clients/Acme", LabelType::User),
    ]
}

fn row_in(view: ThreadView, thread_labels: &[&str]) -> ThreadRow {
    let store = Store::open_in_memory().unwrap();
    put(&store, "t", 1_000, thread_labels);
    let mut list = ThreadList::new(view);
    list.set_labels(labels());
    let first = list.refresh().unwrap();
    complete(&mut list, &store, first);
    list.row(0, date(2_000), |d| d).unwrap()
}

fn chip_names(row: &ThreadRow) -> Vec<&str> {
    row.labels.iter().map(|chip| chip.name.as_str()).collect()
}

#[test]
fn chips_show_user_labels_but_not_the_view_or_system_labels() {
    let row = row_in(
        inbox(),
        &[
            "INBOX",
            "STARRED",
            "CATEGORY_UPDATES",
            "Label_hidden",
            "Label_work",
            "Label_unknown",
        ],
    );
    assert_eq!(chip_names(&row), ["Work"]);
    assert_eq!(
        row.labels[0].color.as_ref().unwrap().background_color,
        "#16a765"
    );
    assert_eq!(row.more_labels, 0);

    // Outside the Inbox, Inbox is a chip; the view's own label is not.
    let row = row_in(view("Label_work"), &["INBOX", "Label_work", "Label_b"]);
    assert_eq!(chip_names(&row), ["Bills", "Inbox"]);
}

#[test]
fn chips_are_sorted_and_capped() {
    let row = row_in(
        ThreadView::AllMail,
        &["Label_work", "Label_c", "Label_b", "Label_a", "INBOX"],
    );
    assert_eq!(chip_names(&row), ["accounts", "Bills", "Clients/Acme"]);
    assert_eq!(row.more_labels, 2);
}

#[test]
fn rows_carry_flags_and_clean_text() {
    let store = Store::open_in_memory().unwrap();
    store
        .upsert_thread(&Thread {
            id: ThreadId::new("t"),
            history_id: HistoryId::new(1),
            subject: Some("  Quarterly\r\n plan ".to_owned()),
            snippet: "Ada&#39;s &quot;plan&quot;\n  &amp; notes".to_owned(),
            last_message_date: date(1_000),
            message_count: 2,
            label_ids: Vec::new(),
            senders: vec![ada()],
            has_attachments: true,
        })
        .unwrap();
    for (n, labels) in [["INBOX", "IMPORTANT"], ["INBOX", "UNREAD"]]
        .iter()
        .enumerate()
    {
        store
            .upsert_message(&Message {
                id: MessageId::new(format!("m{n}")),
                thread_id: ThreadId::new("t"),
                history_id: HistoryId::new(1),
                label_ids: labels.iter().copied().map(LabelId::from).collect(),
                snippet: String::new(),
                internal_date: date(1_000),
                size_estimate: 100,
                headers: MessageHeaders::default(),
                has_attachments: true,
            })
            .unwrap();
    }
    let mut list = ThreadList::default();
    let first = list.refresh().unwrap();
    complete(&mut list, &store, first);

    let row = list.row(0, date(1_000), |d| d).unwrap();
    assert_eq!(row.senders, "Ada Lovelace (2)");
    assert_eq!(row.subject, "Quarterly plan");
    assert_eq!(row.snippet, "Ada's \"plan\" & notes");
    assert!(row.unread);
    assert!(row.important);
    assert!(!row.starred);
    assert!(row.attachment);
    assert_eq!(list.row(1, date(1_000), |d| d), None);
}

#[test]
fn a_missing_subject_says_so() {
    assert_eq!(subject_text(None), "(no subject)");
    assert_eq!(subject_text(Some("  ")), "(no subject)");
    assert_eq!(subject_text(Some("Hi")), "Hi");
}

#[test]
fn snippets_drop_invisible_padding() {
    let padded = "Leather you can feel. \u{200c} \u{200c}\u{34f} \u{feff}\u{a0}&nbsp;\u{200c}";
    assert_eq!(snippet_text(padded), "Leather you can feel.");
    // Joiners inside words are kept (emoji sequences, scripts that use them).
    let family = "Family \u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467} photos";
    assert_eq!(snippet_text(family), family);
}
