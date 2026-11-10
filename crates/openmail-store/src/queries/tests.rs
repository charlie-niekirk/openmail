//! Query tests: paging.

use openmail_core::{
    Address, HistoryId, LabelId, Message, MessageHeaders, MessageId, Thread, ThreadId,
};
use time::OffsetDateTime;

use super::{ThreadCursor, ThreadPage, ThreadView};
use crate::Store;

fn date(secs: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(secs).unwrap()
}

fn label(id: &str) -> ThreadView {
    ThreadView::Label(LabelId::new(id))
}

/// Stores thread `id` with one message per entry of `messages` (its labels), all
/// dated `secs`, oldest first by one second each, ending at `secs`.
fn put_thread(store: &Store, id: &str, secs: i64, messages: &[&[&str]]) {
    let count = i64::try_from(messages.len()).unwrap();
    store
        .upsert_thread(&Thread {
            id: ThreadId::new(id),
            history_id: HistoryId::new(1),
            subject: Some(format!("Subject {id}")),
            snippet: format!("Snippet {id}"),
            last_message_date: date(secs),
            message_count: u32::try_from(messages.len()).unwrap(),
            label_ids: Vec::new(),
            senders: vec![Address::named("Ada", "ada@example.com")],
            has_attachments: false,
        })
        .unwrap();
    for (n, labels) in (0..).zip(messages) {
        store
            .upsert_message(&Message {
                id: MessageId::new(format!("{id}-m{n}")),
                thread_id: ThreadId::new(id),
                history_id: HistoryId::new(1),
                label_ids: labels.iter().copied().map(LabelId::from).collect(),
                snippet: format!("Message {n}"),
                internal_date: date(secs - count + 1 + n),
                size_estimate: 100,
                headers: MessageHeaders {
                    from: vec![Address::named("Ada", "ada@example.com")],
                    subject: Some(format!("Subject {id}")),
                    ..MessageHeaders::default()
                },
                has_attachments: false,
            })
            .unwrap();
    }
}

fn ids(page: &ThreadPage) -> Vec<&str> {
    page.threads.iter().map(|t| t.id.as_str()).collect()
}

/// Every thread in `view`, paging `size` at a time.
fn all_pages(store: &Store, view: &ThreadView, size: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor: Option<ThreadCursor> = None;
    loop {
        let page = store.thread_page(view, cursor.as_ref(), size).unwrap();
        assert!(page.threads.len() <= size);
        out.extend(page.threads.iter().map(|t| t.id.as_str().to_owned()));
        match page.next {
            Some(next) => cursor = Some(next),
            None => return out,
        }
    }
}

#[test]
fn pages_cover_a_label_in_order_without_duplicates_or_gaps() {
    let store = Store::open_in_memory().unwrap();
    // 25 inbox threads; every third shares a date with its neighbour.
    let mut expected = Vec::new();
    for n in 0..25 {
        let id = format!("t{n:02}");
        put_thread(&store, &id, 1_000 + n / 3 * 10, &[&["INBOX"]]);
        expected.push((1_000 + n / 3 * 10, id));
    }
    put_thread(&store, "other", 5_000, &[&["SENT"]]);
    // Newest first, ties by thread ID descending.
    expected.sort_by(|a, b| b.cmp(a));
    let expected: Vec<String> = expected.into_iter().map(|(_, id)| id).collect();

    for size in [1, 2, 3, 7, 25, 100] {
        assert_eq!(
            all_pages(&store, &label("INBOX"), size),
            expected,
            "size {size}"
        );
    }
}

#[test]
fn the_last_full_page_has_no_next_cursor() {
    let store = Store::open_in_memory().unwrap();
    for n in 0..4 {
        put_thread(&store, &format!("t{n}"), 100 + n, &[&["INBOX"]]);
    }
    let first = store.thread_page(&label("INBOX"), None, 2).unwrap();
    assert_eq!(ids(&first), ["t3", "t2"]);
    let second = store
        .thread_page(&label("INBOX"), first.next.as_ref(), 2)
        .unwrap();
    assert_eq!(ids(&second), ["t1", "t0"]);
    assert_eq!(second.next, None);

    let empty = store.thread_page(&label("STARRED"), None, 50).unwrap();
    assert!(empty.threads.is_empty());
    assert_eq!(empty.next, None);
    // A limit of 0 still makes progress.
    assert_eq!(
        ids(&store.thread_page(&label("INBOX"), None, 0).unwrap()),
        ["t3"]
    );
}

#[test]
fn ties_on_date_page_by_thread_id() {
    let store = Store::open_in_memory().unwrap();
    for id in ["a", "b", "c", "d", "e"] {
        put_thread(&store, id, 100, &[&["INBOX"]]);
    }
    let first = store.thread_page(&label("INBOX"), None, 2).unwrap();
    assert_eq!(ids(&first), ["e", "d"]);
    let second = store
        .thread_page(&label("INBOX"), first.next.as_ref(), 2)
        .unwrap();
    assert_eq!(ids(&second), ["c", "b"]);
    let third = store
        .thread_page(&label("INBOX"), second.next.as_ref(), 2)
        .unwrap();
    assert_eq!(ids(&third), ["a"]);
}

#[test]
fn changes_between_pages_neither_repeat_nor_skip_unchanged_threads() {
    let store = Store::open_in_memory().unwrap();
    for n in 0..6 {
        put_thread(&store, &format!("t{n}"), 100 + n, &[&["INBOX"]]);
    }
    let first = store.thread_page(&label("INBOX"), None, 3).unwrap();
    assert_eq!(ids(&first), ["t5", "t4", "t3"]);

    // New mail arrives above the cursor, a shown thread leaves the inbox, an
    // unseen one gets a newer message (moving above the cursor) and another
    // unseen one leaves the inbox.
    put_thread(&store, "new", 200, &[&["INBOX"]]);
    store
        .set_message_labels(&MessageId::new("t4-m0"), &[])
        .unwrap();
    put_thread(&store, "t2", 300, &[&["INBOX"], &["INBOX"]]);
    store
        .set_message_labels(&MessageId::new("t1-m0"), &[LabelId::new("SENT")])
        .unwrap();

    let second = store
        .thread_page(&label("INBOX"), first.next.as_ref(), 3)
        .unwrap();
    assert_eq!(ids(&second), ["t0"]);
    assert_eq!(second.next, None);

    // A fresh listing sees the new order.
    assert_eq!(
        all_pages(&store, &label("INBOX"), 2),
        ["t2", "new", "t5", "t3", "t0"]
    );
}

#[test]
fn summaries_carry_flags_participants_and_labels() {
    let store = Store::open_in_memory().unwrap();
    put_thread(
        &store,
        "t",
        100,
        &[
            &["INBOX", "IMPORTANT"],
            &["INBOX", "UNREAD", "STARRED", "Label_1"],
        ],
    );
    put_thread(&store, "quiet", 50, &[&["INBOX"]]);

    let page = store.thread_page(&label("INBOX"), None, 50).unwrap();
    let [t, quiet] = page.threads.as_slice() else {
        panic!("expected two threads, got {page:?}");
    };
    assert_eq!(t.id.as_str(), "t");
    assert_eq!(t.subject.as_deref(), Some("Subject t"));
    assert_eq!(t.snippet, "Snippet t");
    assert_eq!(t.last_message_date, date(100));
    assert_eq!(t.participants, [Address::named("Ada", "ada@example.com")]);
    assert_eq!(t.message_count, 2);
    assert!(t.is_unread && t.is_starred && t.is_important && !t.has_attachments);
    let labels: Vec<&str> = t.label_ids.iter().map(LabelId::as_str).collect();
    assert_eq!(
        labels,
        ["IMPORTANT", "INBOX", "Label_1", "STARRED", "UNREAD"]
    );

    assert!(!quiet.is_unread && !quiet.is_starred && !quiet.is_important);
    assert_eq!(quiet.label_ids, [LabelId::new("INBOX")]);
}

#[test]
fn all_of_requires_every_label() {
    let store = Store::open_in_memory().unwrap();
    put_thread(&store, "social", 300, &[&["INBOX", "CATEGORY_SOCIAL"]]);
    put_thread(&store, "split", 200, &[&["INBOX"], &["CATEGORY_SOCIAL"]]);
    put_thread(&store, "archived", 100, &[&["CATEGORY_SOCIAL"]]);
    put_thread(&store, "primary", 50, &[&["INBOX", "CATEGORY_PERSONAL"]]);

    let view = ThreadView::AllOf(vec![LabelId::new("INBOX"), LabelId::new("CATEGORY_SOCIAL")]);
    // Thread labels are the union over messages, as in Gmail's thread view.
    assert_eq!(all_pages(&store, &view, 1), ["social", "split"]);
    assert_eq!(
        all_pages(&store, &ThreadView::AllOf(vec![LabelId::new("INBOX")]), 10),
        all_pages(&store, &label("INBOX"), 10)
    );
    assert!(all_pages(&store, &ThreadView::AllOf(Vec::new()), 10).is_empty());
}

#[test]
fn all_mail_leaves_out_threads_entirely_in_spam_or_trash() {
    let store = Store::open_in_memory().unwrap();
    put_thread(&store, "inbox", 600, &[&["INBOX"]]);
    put_thread(&store, "spam", 500, &[&["SPAM"]]);
    put_thread(&store, "archived", 400, &[&[]]);
    put_thread(&store, "trash", 300, &[&["TRASH"], &["TRASH", "UNREAD"]]);
    put_thread(&store, "partly", 200, &[&["TRASH"], &["SENT"]]);
    put_thread(&store, "sent", 100, &[&["SENT"]]);

    assert_eq!(
        all_pages(&store, &ThreadView::AllMail, 2),
        ["inbox", "archived", "partly", "sent"]
    );
}
