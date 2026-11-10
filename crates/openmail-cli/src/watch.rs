//! `watch`: keep the local store in sync through `MailService`, polling
//! Gmail's history, sending queued ops and prefetching recent bodies.

use std::io::Write as _;
use std::time::Duration;

use anyhow::Context as _;
use openmail_gmail::GmailClient;
use openmail_sync::{
    Changes, Connectivity, MailEvent, MailEvents, MailService, MailServiceConfig, SchedulerConfig,
    SyncStatus,
};
use time::{OffsetDateTime, UtcOffset};

use crate::ops::event_line;
use crate::sync::{cancel_on_ctrl_c, open_store, progress_line};

/// Runs the mail service for `account` until Ctrl-C: the initial sync if it
/// has not finished (the last 90 days), then a poll every `interval`, with a
/// line per poll, per batch of prefetched bodies and per op sent.
pub async fn run(
    client: GmailClient,
    account: &str,
    interval: Duration,
    offset: UtcOffset,
) -> anyhow::Result<()> {
    // Creates the database and records the account.
    let (store, path) = open_store(account)?;
    drop(store);
    println!(
        "Watching {account} ({}), polling every {}s. Ctrl-C stops.",
        path.display(),
        interval.as_secs()
    );
    let config = MailServiceConfig {
        scheduler: SchedulerConfig {
            active_interval: interval,
            ..SchedulerConfig::default()
        },
        ..MailServiceConfig::new(&path)
    };
    let (service, mut events) =
        MailService::start(client, config).context("could not start the mail service")?;
    let cancel = cancel_on_ctrl_c();
    let mut printer = Printer {
        offset,
        in_progress: false,
    };
    let result = loop {
        let event = tokio::select! {
            () = cancel.cancelled() => break Ok(()),
            event = events.recv() => event,
        };
        let Some(event) = event else {
            break Ok(());
        };
        if let Some(result) = printer.print(event, &mut events) {
            break result;
        }
    };
    tokio::task::spawn_blocking(move || service.shutdown())
        .await
        .context("the mail service did not shut down")?;
    if result.is_ok() {
        println!("Stopped.");
    }
    result
}

/// Prints service events, one line each.
struct Printer {
    offset: UtcOffset,
    /// Whether the last line is an initial-sync progress line, rewritten in
    /// place.
    in_progress: bool,
}

impl Printer {
    /// Prints `event`; `Some` when watching should stop.
    fn print(&mut self, event: MailEvent, events: &mut MailEvents) -> Option<anyhow::Result<()>> {
        let time = clock(self.offset);
        if let MailEvent::SyncStatus(SyncStatus::Initial(progress)) = &event {
            let mut stderr = std::io::stderr();
            let _ = write!(stderr, "\r{time} {}\x1b[K", progress_line(progress));
            let _ = stderr.flush();
            self.in_progress = true;
            return None;
        }
        let line = match event {
            MailEvent::SyncStatus(SyncStatus::UpToDate(changes)) => poll_line(&changes),
            MailEvent::SyncStatus(SyncStatus::Retrying { error, retry_in }) => format!(
                "failed: {}; retrying in {}s",
                error_chain(&*error),
                retry_in.as_secs()
            ),
            MailEvent::SyncStatus(SyncStatus::Stopped(error)) => {
                self.end_progress();
                return Some(Err(anyhow::anyhow!(error_chain(&*error))).context("syncing stopped"));
            }
            MailEvent::Connectivity(Connectivity::Online) => "online".to_owned(),
            MailEvent::Connectivity(Connectivity::Offline) => "offline".to_owned(),
            MailEvent::BodyFetched(_) => {
                // Bodies arrive in batches: count those already waiting.
                let mut count = 1;
                while let Some(next) = events.try_recv() {
                    if matches!(next, MailEvent::BodyFetched(_)) {
                        count += 1;
                    } else {
                        self.print_line(&time, &bodies_line(count));
                        return self.print(next, events);
                    }
                }
                bodies_line(count)
            }
            MailEvent::Op(event) => event_line(&event),
            MailEvent::OpsStopped(error) => {
                format!("sending ops stopped: {}", error_chain(&*error))
            }
            _ => return None,
        };
        self.print_line(&time, &line);
        None
    }

    fn print_line(&mut self, time: &str, line: &str) {
        self.end_progress();
        println!("{time} {line}");
    }

    fn end_progress(&mut self) {
        if std::mem::take(&mut self.in_progress) {
            eprintln!();
        }
    }
}

/// `1 body fetched` or `N bodies fetched`.
fn bodies_line(count: usize) -> String {
    if count == 1 {
        "1 body fetched".to_owned()
    } else {
        format!("{count} bodies fetched")
    }
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

    #[test]
    fn counts_bodies() {
        assert_eq!(bodies_line(1), "1 body fetched");
        assert_eq!(bodies_line(10), "10 bodies fetched");
    }
}
