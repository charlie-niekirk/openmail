//! Read-query benchmarks over a synthetic mailbox of 100k threads.
//!
//! Run with `cargo bench -p openmail-store`. Seeding the temporary database takes
//! a while before the first measurement.

use std::hint::black_box;
use std::time::Instant;

use criterion::{Criterion, criterion_group, criterion_main};
use openmail_core::{
    Address, HistoryId, LabelId, Message, MessageHeaders, MessageId, Thread, ThreadId,
};
use openmail_store::{Error, MessageBody, Store, ThreadCursor, ThreadView};
use time::OffsetDateTime;

const THREADS: u32 = 100_000;
const PAGE: usize = 50;
/// How many threads deep the deep-page benchmark starts.
const DEEP: usize = 20_000;

/// The 12 user labels; with the 8 system labels, 20 in all.
const USER_LABELS: [&str; 12] = [
    "CATEGORY_PERSONAL",
    "CATEGORY_SOCIAL",
    "CATEGORY_PROMOTIONS",
    "CATEGORY_UPDATES",
    "CATEGORY_FORUMS",
    "Label_1",
    "Label_2",
    "Label_3",
    "Label_4",
    "Label_5",
    "Label_6",
    "Label_7",
];

/// A small deterministic generator (xorshift64*), so every run seeds the same mailbox.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// True with probability `percent` / 100.
    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % n as u64).unwrap_or(0)
    }
}

/// Fills `store` with [`THREADS`] threads of 1–4 messages (about 250k in all).
///
/// A third of threads are in the inbox, 10% unread, 20% important, 3% starred,
/// 3% in spam or trash; every thread has a category and 30% a user label. Every
/// tenth thread has its bodies fetched. Dates step by a minute with frequent ties.
fn seed(store: &Store) -> Result<(), Error> {
    let mut rng = Rng(0x0DDB_1A5E_5BAD_5EED);
    let base = 1_600_000_000;
    let sender = Address::named("Ada Lovelace", "ada@example.com");
    store.transaction(|store| {
        for n in 0..THREADS {
            let id = format!("{:016x}", 0x1000_0000_u64 + u64::from(n));
            let last = base + i64::from(n / 2) * 60;
            let count = rng.below(4) + 1;

            let mut thread_labels = vec![USER_LABELS[rng.below(5)]];
            if rng.chance(30) {
                thread_labels.push(USER_LABELS[5 + rng.below(7)]);
            }
            if rng.chance(3) {
                thread_labels.push(if rng.chance(33) { "SPAM" } else { "TRASH" });
            } else if rng.chance(33) {
                thread_labels.push("INBOX");
            }
            if rng.chance(20) {
                thread_labels.push("IMPORTANT");
            }
            let unread = rng.chance(10);
            let starred = rng.chance(3);

            store.upsert_thread(&Thread {
                id: ThreadId::new(&id),
                history_id: HistoryId::new(u64::from(n) + 1),
                subject: Some(format!("Thread {n} about the quarterly numbers")),
                snippet: "Here are the figures we discussed on Tuesday, plus a few notes"
                    .to_owned(),
                last_message_date: date(last),
                message_count: u32::try_from(count).unwrap_or(1),
                label_ids: Vec::new(),
                senders: vec![sender.clone()],
                has_attachments: n % 7 == 0,
            })?;

            for m in 0..count {
                let is_last = m + 1 == count;
                let mut labels: Vec<LabelId> =
                    thread_labels.iter().copied().map(LabelId::from).collect();
                if m % 2 == 1 {
                    labels.push(LabelId::new("SENT"));
                }
                if is_last && unread {
                    labels.push(LabelId::new("UNREAD"));
                }
                if is_last && starred {
                    labels.push(LabelId::new("STARRED"));
                }
                let message_id = MessageId::new(format!("{id}{m:x}"));
                let back = i64::try_from(count - 1 - m).unwrap_or(0) * 600;
                store.upsert_message(&Message {
                    id: message_id.clone(),
                    thread_id: ThreadId::new(&id),
                    history_id: HistoryId::new(u64::from(n) + 1),
                    label_ids: labels,
                    snippet: "Here are the figures we discussed".to_owned(),
                    internal_date: date(last - back),
                    size_estimate: 12_000,
                    headers: MessageHeaders {
                        from: vec![sender.clone()],
                        to: vec![Address::new("charles@example.com")],
                        subject: Some(format!("Thread {n} about the quarterly numbers")),
                        ..MessageHeaders::default()
                    },
                    has_attachments: n % 7 == 0,
                })?;
                if n % 10 == 0 {
                    store.upsert_body(
                        &message_id,
                        &MessageBody {
                            text: Some("Hi Charles,\n\nThe numbers are attached.\n".repeat(10)),
                            html: Some("<p>Hi Charles,</p><p>The numbers.</p>".repeat(10)),
                            fetched_at: date(last),
                            inline: Vec::new(),
                        },
                    )?;
                }
            }
        }
        Ok(())
    })
}

fn date(secs: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(secs).unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

/// The cursor `depth` threads into `view`.
fn cursor_at(store: &Store, view: &ThreadView, depth: usize) -> ThreadCursor {
    let mut cursor = None;
    let mut seen = 0;
    while seen < depth {
        let page = store
            .thread_page(view, cursor.as_ref(), 1_000.min(depth - seen))
            .expect("page");
        seen += page.threads.len();
        cursor = Some(page.next.expect("the view is deep enough"));
    }
    cursor.expect("depth is positive")
}

fn queries(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("temporary directory");
    let store = Store::open(&dir.path().join("mail.db")).expect("open store");
    let started = Instant::now();
    seed(&store).expect("seed");
    eprintln!("seeded {THREADS} threads in {:.1?}", started.elapsed());

    let inbox = ThreadView::Label(LabelId::new("INBOX"));
    let deep = cursor_at(&store, &inbox, DEEP);
    let all_mail_deep = cursor_at(&store, &ThreadView::AllMail, DEEP);
    let primary = ThreadView::AllOf(vec![
        LabelId::new("INBOX"),
        LabelId::new("CATEGORY_PERSONAL"),
    ]);
    // A four-message thread with bodies.
    let detail = ThreadId::new(format!("{:016x}", 0x1000_0000_u64 + 30));
    assert_eq!(
        store
            .thread_detail(&detail)
            .expect("detail")
            .map(|d| d.messages.len()),
        Some(4)
    );

    let mut group = c.benchmark_group("queries");
    group.bench_function("inbox first page", |b| {
        b.iter(|| {
            store
                .thread_page(black_box(&inbox), None, PAGE)
                .expect("page")
        });
    });
    group.bench_function("inbox deep page", |b| {
        b.iter(|| {
            store
                .thread_page(black_box(&inbox), Some(&deep), PAGE)
                .expect("page")
        });
    });
    group.bench_function("primary tab first page", |b| {
        b.iter(|| {
            store
                .thread_page(black_box(&primary), None, PAGE)
                .expect("page")
        });
    });
    group.bench_function("all mail first page", |b| {
        b.iter(|| {
            store
                .thread_page(black_box(&ThreadView::AllMail), None, PAGE)
                .expect("page")
        });
    });
    group.bench_function("all mail deep page", |b| {
        b.iter(|| {
            store
                .thread_page(black_box(&ThreadView::AllMail), Some(&all_mail_deep), PAGE)
                .expect("page")
        });
    });
    group.bench_function("label counts", |b| {
        b.iter(|| store.label_counts().expect("counts"));
    });
    group.bench_function("thread detail", |b| {
        b.iter(|| store.thread_detail(black_box(&detail)).expect("detail"));
    });
    group.finish();
}

criterion_group!(benches, queries);
criterion_main!(benches);
