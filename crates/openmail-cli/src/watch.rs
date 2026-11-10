//! `watch`: keep the local store in sync, polling Gmail's history.

use std::io::Write as _;
use std::time::Duration;

use anyhow::Context as _;
use openmail_gmail::GmailClient;
use openmail_sync::{
    Changes, Connectivity, InitialSyncConfig, Scheduler, SchedulerConfig, SyncEvent,
};
use time::{OffsetDateTime, UtcOffset};

use crate::sync::{cancel_on_ctrl_c, open_store, progress_line};

/// Runs the sync scheduler for `account` until Ctrl-C: the initial sync if it
/// has not finished (the last 90 days), then a poll every `interval`, with a
/// line per poll.
pub async fn run(
    client: GmailClient,
    account: &str,
    interval: Duration,
    offset: UtcOffset,
) -> anyhow::Result<()> {
    let (store, path) = open_store(account)?;
    println!(
        "Watching {account} ({}), polling every {}s. Ctrl-C stops.",
        path.display(),
        interval.as_secs()
    );
    let config = SchedulerConfig {
        active_interval: interval,
        ..SchedulerConfig::default()
    };
    let initial = InitialSyncConfig::default();
    let mut scheduler = Scheduler::new(client, store, initial, config);
    let cancel = cancel_on_ctrl_c();
    let mut stderr = std::io::stderr();
    let mut in_progress = false;
    scheduler
        .run(&cancel, |event| {
            let time = clock(offset);
            match event {
                SyncEvent::Initial(progress) => {
                    let _ = write!(stderr, "\r{time} {}\x1b[K", progress_line(&progress));
                    let _ = stderr.flush();
                    in_progress = true;
                }
                event => {
                    if std::mem::take(&mut in_progress) {
                        eprintln!();
                    }
                    match event {
                        SyncEvent::Polled(changes) => println!("{time} {}", poll_line(&changes)),
                        SyncEvent::Connectivity(Connectivity::Online) => {
                            println!("{time} online");
                        }
                        SyncEvent::Connectivity(Connectivity::Offline) => {
                            println!("{time} offline");
                        }
                        SyncEvent::Failed { error, retry_in } => println!(
                            "{time} failed: {}; retrying in {}s",
                            error_chain(&*error),
                            retry_in.as_secs()
                        ),
                        _ => {}
                    }
                }
            }
        })
        .await
        .context("syncing stopped")?;
    println!("Stopped.");
    Ok(())
}

/// An error and its sources, joined with `: `.
pub fn error_chain(err: &dyn std::error::Error) -> String {
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(err) = source {
        text.push_str(": ");
        text.push_str(&err.to_string());
        source = err.source();
    }
    text
}

/// The local time as `HH:MM:SS`.
fn clock(offset: UtcOffset) -> String {
    let now = OffsetDateTime::now_utc().to_offset(offset);
    format!("{:02}:{:02}:{:02}", now.hour(), now.minute(), now.second())
}

/// One line describing a poll, such as
/// `2 threads fetched, 3 label changes (history 12345)`.
pub fn poll_line(changes: &Changes) -> String {
    if changes.full_resync {
        return "history expired; starting a full resync".to_owned();
    }
    let mut parts = Vec::new();
    let mut count = |n: u64, one: &str, many: &str| {
        if n > 0 {
            parts.push(format!("{n} {}", if n == 1 { one } else { many }));
        }
    };
    count(changes.threads_fetched, "thread fetched", "threads fetched");
    count(changes.threads_deleted, "thread deleted", "threads deleted");
    count(changes.label_changes, "label change", "label changes");
    if changes.labels_synced {
        parts.push("labels re-synced".to_owned());
    }
    let summary = if parts.is_empty() {
        "no changes".to_owned()
    } else {
        parts.join(", ")
    };
    match changes.history_id {
        Some(id) => format!("{summary} (history {id})"),
        None => summary,
    }
}

#[cfg(test)]
mod tests {
    use openmail_core::HistoryId;

    use super::*;

    #[test]
    fn formats_the_poll_line() {
        let mut changes = Changes {
            history_id: Some(HistoryId::new(12345)),
            ..Changes::default()
        };
        assert_eq!(poll_line(&changes), "no changes (history 12345)");

        changes.threads_fetched = 2;
        changes.threads_deleted = 1;
        changes.label_changes = 3;
        changes.labels_synced = true;
        assert_eq!(
            poll_line(&changes),
            "2 threads fetched, 1 thread deleted, 3 label changes, labels re-synced \
             (history 12345)"
        );

        changes.full_resync = true;
        assert_eq!(
            poll_line(&changes),
            "history expired; starting a full resync"
        );
    }
}
