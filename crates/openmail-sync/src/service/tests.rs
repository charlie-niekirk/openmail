use openmail_core::LabelId;

use super::*;
use crate::{Phase, Target};

fn collect(receiver: &mut mpsc::UnboundedReceiver<MailEvent>) -> Vec<String> {
    let mut seen = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        seen.push(match event {
            MailEvent::SyncStatus(SyncStatus::Initial(p)) => format!("initial {:?}", p.phase),
            MailEvent::SyncStatus(SyncStatus::UpToDate(_)) => "up to date".to_owned(),
            MailEvent::SyncStatus(SyncStatus::Retrying { .. }) => "retrying".to_owned(),
            MailEvent::SyncStatus(SyncStatus::Stopped(_)) => "stopped".to_owned(),
            MailEvent::Connectivity(c) => format!("{c:?}"),
            MailEvent::ThreadsChanged => "threads".to_owned(),
            MailEvent::LabelsChanged => "labels".to_owned(),
            MailEvent::BodyFetched(id) => format!("body {id}"),
            MailEvent::Op(_) => "op".to_owned(),
            MailEvent::OpsStopped(_) => "ops stopped".to_owned(),
        });
    }
    seen
}

#[test]
fn initial_progress_reports_what_changed() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let events = Events(tx);
    let mut last = None;
    let mut progress = Progress::default();
    report_sync(&events, &mut last, SyncEvent::Initial(progress.clone()));
    assert_eq!(collect(&mut rx), ["initial Labels"]);

    progress.phase = Phase::Inbox;
    progress.labels = 3;
    report_sync(&events, &mut last, SyncEvent::Initial(progress.clone()));
    assert_eq!(collect(&mut rx), ["labels", "initial Inbox"]);

    progress.threads = 10;
    report_sync(&events, &mut last, SyncEvent::Initial(progress.clone()));
    assert_eq!(collect(&mut rx), ["threads", "initial Inbox"]);

    // A report that changed nothing in the store.
    progress.phase = Phase::Backfill;
    report_sync(&events, &mut last, SyncEvent::Initial(progress.clone()));
    assert_eq!(collect(&mut rx), ["initial Backfill"]);
}

#[test]
fn polls_report_changes_and_status() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let events = Events(tx);
    let mut last = None;
    report_sync(&events, &mut last, SyncEvent::Polled(Changes::default()));
    assert_eq!(collect(&mut rx), ["up to date"]);

    let changes = Changes {
        label_changes: 2,
        labels_synced: true,
        ..Changes::default()
    };
    report_sync(&events, &mut last, SyncEvent::Polled(changes));
    assert_eq!(collect(&mut rx), ["labels", "threads", "up to date"]);

    let resync = Changes {
        full_resync: true,
        ..Changes::default()
    };
    report_sync(&events, &mut last, SyncEvent::Polled(resync));
    assert_eq!(collect(&mut rx), ["threads"]);

    report_sync(
        &events,
        &mut last,
        SyncEvent::Failed {
            error: Arc::new(Error::Cancelled),
            retry_in: Duration::from_secs(5),
        },
    );
    report_sync(
        &events,
        &mut last,
        SyncEvent::Connectivity(Connectivity::Offline),
    );
    assert_eq!(collect(&mut rx), ["retrying", "Offline"]);
}

#[test]
fn ops_invalidate_what_they_change() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let events = Events(tx);
    let label = LabelId::new("Label_1");
    Changed::by(&Op::archive(Target::thread(ThreadId::new("t1")))).report(&events);
    assert_eq!(collect(&mut rx), ["threads"]);
    Changed::by(&Op::RenameLabel {
        label: label.clone(),
        name: "Work".to_owned(),
    })
    .report(&events);
    assert_eq!(collect(&mut rx), ["labels"]);
    Changed::by(&Op::DeleteLabel { label }).report(&events);
    assert_eq!(collect(&mut rx), ["labels", "threads"]);
}
