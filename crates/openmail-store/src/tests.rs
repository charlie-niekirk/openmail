//! Store behaviour tests, on in-memory and temporary-file databases.

use openmail_core::{
    Account, AccountId, Address, HistoryId, Label, LabelColor, LabelId, LabelListVisibility,
    LabelType, Message, MessageHeaders, MessageId, MessageListVisibility, Thread, ThreadId,
};
use time::OffsetDateTime;

use crate::{DataDir, Error, MessageBody, Store, SyncState};

fn date(secs: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(secs).unwrap()
}

fn labels(ids: &[&str]) -> Vec<LabelId> {
    ids.iter().copied().map(LabelId::from).collect()
}

fn thread(id: &str, last: i64) -> Thread {
    Thread {
        id: ThreadId::new(id),
        history_id: HistoryId::new(100),
        subject: Some("Quarterly report".to_owned()),
        snippet: "Numbers attached".to_owned(),
        last_message_date: date(last),
        message_count: 2,
        label_ids: Vec::new(),
        senders: vec![Address::named("Ada Lovelace", "ada@example.com")],
        has_attachments: true,
    }
}

fn message(id: &str, thread: &str, label_ids: &[&str]) -> Message {
    Message {
        id: MessageId::new(id),
        thread_id: ThreadId::new(thread),
        history_id: HistoryId::new(101),
        label_ids: labels(label_ids),
        snippet: "Numbers attached".to_owned(),
        internal_date: OffsetDateTime::from_unix_timestamp_nanos(1_700_000_000_123_000_000)
            .unwrap(),
        size_estimate: 4096,
        headers: MessageHeaders {
            from: vec![Address::named("Ada Lovelace", "ada@example.com")],
            to: vec![Address::new("charles@example.com")],
            cc: vec![Address::named("Mary Somerville", "mary@example.com")],
            bcc: Vec::new(),
            reply_to: vec![Address::new("reports@example.com")],
            subject: Some("Quarterly report".to_owned()),
            message_id_header: Some("<abc@mail.example.com>".to_owned()),
            in_reply_to: None,
            references: None,
        },
        has_attachments: true,
    }
}

/// The store's denormalized flags for a thread: (is_unread, is_starred).
fn flags(store: &Store, thread: &str) -> (bool, bool) {
    store
        .conn()
        .query_row(
            "SELECT is_unread, is_starred FROM threads WHERE id = ?1",
            [thread],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
}

/// thread_labels rows for a thread: (label, message_count, last_message_date).
fn thread_label_rows(store: &Store, thread: &str) -> Vec<(String, i64, i64)> {
    let mut stmt = store
        .conn()
        .prepare(
            "SELECT label_id, message_count, last_message_date FROM thread_labels
             WHERE thread_id = ?1 ORDER BY label_id",
        )
        .unwrap();
    stmt.query_map([thread], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

#[test]
fn account_round_trips() {
    let store = Store::open_in_memory().unwrap();
    let mut account = Account {
        id: AccountId::new("ada@example.com"),
        email: "ada@example.com".to_owned(),
        display_name: None,
    };
    store.upsert_account(&account).unwrap();
    account.display_name = Some("Ada Lovelace".to_owned());
    store.upsert_account(&account).unwrap();
    assert_eq!(store.account(&account.id).unwrap(), Some(account));
    assert_eq!(store.account(&AccountId::new("other")).unwrap(), None);
}

#[test]
fn labels_round_trip_and_update() {
    let store = Store::open_in_memory().unwrap();
    let inbox = Label {
        id: LabelId::new("INBOX"),
        name: "INBOX".to_owned(),
        label_type: LabelType::System,
        color: None,
        label_list_visibility: LabelListVisibility::LabelShow,
        message_list_visibility: MessageListVisibility::Hide,
    };
    let mut work = Label {
        id: LabelId::new("Label_1"),
        name: "Work".to_owned(),
        label_type: LabelType::User,
        color: Some(LabelColor {
            text_color: "#ffffff".to_owned(),
            background_color: "#16a765".to_owned(),
        }),
        label_list_visibility: LabelListVisibility::LabelShowIfUnread,
        message_list_visibility: MessageListVisibility::Show,
    };
    store.upsert_label(&work).unwrap();
    store.upsert_label(&inbox).unwrap();
    assert_eq!(store.label(&work.id).unwrap(), Some(work.clone()));

    work.name = "Work/Projects".to_owned();
    work.color = None;
    work.label_list_visibility = LabelListVisibility::LabelHide;
    store.upsert_label(&work).unwrap();
    // System labels sort first.
    assert_eq!(store.labels().unwrap(), [inbox, work.clone()]);

    assert!(store.delete_label(&work.id).unwrap());
    assert!(!store.delete_label(&work.id).unwrap());
    assert_eq!(store.label(&work.id).unwrap(), None);
}

#[test]
fn threads_and_messages_round_trip() {
    let store = Store::open_in_memory().unwrap();
    let mut t = thread("t1", 1_700_000_000);
    store.upsert_thread(&t).unwrap();

    let m = message("m1", "t1", &["INBOX", "Label_1", "UNREAD"]);
    store.upsert_message(&m).unwrap();
    assert_eq!(store.message(&m.id).unwrap(), Some(m.clone()));
    assert_eq!(store.message(&MessageId::new("nope")).unwrap(), None);

    // A thread's labels come from its messages.
    t.label_ids = labels(&["INBOX", "Label_1", "UNREAD"]);
    assert_eq!(store.thread(&t.id).unwrap(), Some(t.clone()));

    // Updating replaces fields.
    t.snippet = "Updated".to_owned();
    t.message_count = 3;
    store.upsert_thread(&t).unwrap();
    assert_eq!(store.thread(&t.id).unwrap().unwrap().snippet, "Updated");
    assert_eq!(store.thread(&ThreadId::new("nope")).unwrap(), None);
}

#[test]
fn message_needs_its_thread() {
    let store = Store::open_in_memory().unwrap();
    let err = store
        .upsert_message(&message("m1", "missing", &[]))
        .unwrap_err();
    assert!(matches!(err, Error::Sqlite(_)), "{err:?}");
    // The failed upsert left nothing behind.
    assert_eq!(store.message(&MessageId::new("m1")).unwrap(), None);
}

#[test]
fn setting_labels_on_a_missing_message_fails() {
    let store = Store::open_in_memory().unwrap();
    let err = store
        .set_message_labels(&MessageId::new("m1"), &labels(&["INBOX"]))
        .unwrap_err();
    assert!(matches!(err, Error::NotFound(_)), "{err:?}");
}

#[test]
fn thread_labels_follow_message_labels() {
    let store = Store::open_in_memory().unwrap();
    let last = 1_700_000_000;
    store.upsert_thread(&thread("t1", last)).unwrap();
    store
        .upsert_message(&message("m1", "t1", &["INBOX", "UNREAD"]))
        .unwrap();
    store
        .upsert_message(&message("m2", "t1", &["INBOX", "STARRED"]))
        .unwrap();

    let ms = last * 1000;
    assert_eq!(
        thread_label_rows(&store, "t1"),
        [
            ("INBOX".to_owned(), 2, ms),
            ("STARRED".to_owned(), 1, ms),
            ("UNREAD".to_owned(), 1, ms),
        ]
    );
    assert_eq!(flags(&store, "t1"), (true, true));

    // Reading m1 removes UNREAD from the thread; INBOX stays while m2 has it.
    store
        .set_message_labels(&MessageId::new("m1"), &labels(&["INBOX"]))
        .unwrap();
    assert_eq!(flags(&store, "t1"), (false, true));
    assert_eq!(
        store.thread_label_ids(&ThreadId::new("t1")).unwrap(),
        labels(&["INBOX", "STARRED"])
    );

    // A newer message moves the thread's date in its label rows too.
    store.upsert_thread(&thread("t1", last + 60)).unwrap();
    assert!(
        thread_label_rows(&store, "t1")
            .iter()
            .all(|(_, _, date)| *date == (last + 60) * 1000)
    );

    // Deleting a message drops its labels from the thread.
    assert!(store.delete_message(&MessageId::new("m2")).unwrap());
    assert_eq!(
        thread_label_rows(&store, "t1"),
        [("INBOX".to_owned(), 1, (last + 60) * 1000)]
    );
    assert_eq!(flags(&store, "t1"), (false, false));

    // Deleting a label removes it from messages and threads.
    store.delete_label(&LabelId::new("INBOX")).unwrap();
    assert!(
        store
            .message_label_ids(&MessageId::new("m1"))
            .unwrap()
            .is_empty()
    );
    assert!(thread_label_rows(&store, "t1").is_empty());
}

#[test]
fn bodies_round_trip() {
    let store = Store::open_in_memory().unwrap();
    store.upsert_thread(&thread("t1", 1_700_000_000)).unwrap();
    store.upsert_message(&message("m1", "t1", &[])).unwrap();
    let id = MessageId::new("m1");
    assert_eq!(store.body(&id).unwrap(), None);

    let body = MessageBody {
        text: Some("Hello".to_owned()),
        html: Some("<p>Hello</p>".to_owned()),
        fetched_at: date(1_700_000_100),
    };
    store.upsert_body(&id, &body).unwrap();
    assert_eq!(store.body(&id).unwrap(), Some(body));

    // Bodies go with their message.
    store.delete_message(&id).unwrap();
    assert_eq!(store.body(&id).unwrap(), None);
}

#[test]
fn sync_state_round_trips() {
    let store = Store::open_in_memory().unwrap();
    assert_eq!(store.sync_state().unwrap(), SyncState::default());
    let state = SyncState {
        history_id: Some(HistoryId::new(123_456)),
        backfill_cursor: Some("page-7".to_owned()),
        backfill_complete: true,
        last_full_sync: Some(date(1_700_000_000)),
    };
    store.set_sync_state(&state).unwrap();
    assert_eq!(store.sync_state().unwrap(), state);
}

#[test]
fn search_finds_subject_people_and_body() {
    let store = Store::open_in_memory().unwrap();
    store.upsert_thread(&thread("t1", 1_700_000_000)).unwrap();
    store.upsert_message(&message("m1", "t1", &[])).unwrap();
    let mut other = message("m2", "t1", &[]);
    other.headers.subject = Some("Lunch on Friday?".to_owned());
    other.headers.from = vec![Address::named("Grace Hopper", "grace@example.org")];
    other.headers.cc.clear();
    store.upsert_message(&other).unwrap();

    let search = |q: &str| store.search_messages(q, 10).unwrap();
    let m1 = || vec![MessageId::new("m1")];
    let m2 = || vec![MessageId::new("m2")];

    assert_eq!(search("quarterly"), m1());
    assert_eq!(search("sender:grace"), m2());
    assert_eq!(search("sender:hopper"), m2());
    // Recipients cover To and Cc.
    assert_eq!(search("recipients:somerville"), m1());
    assert_eq!(search("charles").len(), 2);
    // Diacritics are folded.
    assert_eq!(search("frīday"), m2());
    assert!(search("budget").is_empty());

    // Bodies are indexed once fetched, and re-indexed when they change.
    let body = |text: &str| MessageBody {
        text: Some(text.to_owned()),
        html: None,
        fetched_at: date(1_700_000_100),
    };
    store
        .upsert_body(&MessageId::new("m2"), &body("The budget is attached"))
        .unwrap();
    assert_eq!(search("budget"), m2());
    store
        .upsert_body(&MessageId::new("m2"), &body("Nothing to see"))
        .unwrap();
    assert!(search("budget").is_empty());

    // Header changes re-index the message.
    other.headers.subject = Some("Dinner instead".to_owned());
    store.upsert_message(&other).unwrap();
    assert!(search("lunch").is_empty());
    assert_eq!(search("dinner"), m2());

    // Deleted messages leave the index.
    store.delete_message(&MessageId::new("m2")).unwrap();
    assert!(search("dinner").is_empty());
    let indexed: i64 = store
        .conn()
        .query_row("SELECT count(*) FROM messages_fts", [], |row| row.get(0))
        .unwrap();
    assert_eq!(indexed, 1);

    assert!(store.search_messages("\"unterminated", 10).is_err());
}

#[test]
fn data_persists_in_a_file_database() {
    let dir = tempfile::tempdir().unwrap();
    let data = DataDir::at(dir.path());
    let account = AccountId::new("ada@example.com");
    {
        let store = Store::open_account(&data, &account).unwrap();
        store.upsert_thread(&thread("t1", 1_700_000_000)).unwrap();
        store
            .upsert_message(&message("m1", "t1", &["INBOX"]))
            .unwrap();
    }
    let store = Store::open_account(&data, &account).unwrap();
    assert_eq!(
        store.thread_label_ids(&ThreadId::new("t1")).unwrap(),
        labels(&["INBOX"])
    );
    assert_eq!(store.search_messages("quarterly", 10).unwrap().len(), 1);
}
