//! `sync`: the initial sync into the account's local store.

use std::io::Write as _;
use std::path::PathBuf;

use anyhow::Context as _;
use openmail_core::{Account, AccountId};
use openmail_gmail::GmailClient;
use openmail_store::{DataDir, Store};
use openmail_sync::{CancellationToken, Cutoff, InitialSync, InitialSyncConfig, Phase, Progress};

/// Runs (or resumes) the initial sync of `account` with a progress line on
/// stderr. Ctrl-C stops it after saving progress.
pub async fn run(
    client: GmailClient,
    account: &str,
    cutoff: Cutoff,
    prefetch: Option<usize>,
) -> anyhow::Result<()> {
    let (store, path) = open_store(account)?;
    if store.sync_state()?.backfill_complete {
        println!(
            "The initial sync of {account} is already complete ({}).",
            path.display()
        );
        return Ok(());
    }

    let cancel = cancel_on_ctrl_c();
    let config = InitialSyncConfig {
        cutoff,
        prefetch_inbox_bodies: prefetch,
        ..InitialSyncConfig::default()
    };
    let mut sync = InitialSync::new(client, store, config);
    let mut stderr = std::io::stderr();
    let result = sync
        .run(&cancel, |progress| {
            // Clear to the end of the line, since lines can get shorter.
            let _ = write!(stderr, "\r{}\x1b[K", progress_line(progress));
            let _ = stderr.flush();
        })
        .await;
    eprintln!();

    match result {
        Ok(done) => {
            println!(
                "Synced {} labels and {} threads ({} messages) into {}",
                done.labels,
                done.threads,
                done.messages,
                path.display()
            );
            Ok(())
        }
        Err(openmail_sync::Error::Cancelled) => {
            println!("Stopped. Run `openmail-cli sync` again to continue.");
            Ok(())
        }
        Err(err) => Err(err).context("the sync stopped; run it again to continue"),
    }
}

/// Opens (creating if needed) the local store of `account`, and returns it
/// with its path.
pub fn open_store(account: &str) -> anyhow::Result<(Store, PathBuf)> {
    let data_dir = DataDir::from_env_or_platform().context("could not find the data directory")?;
    let id = AccountId::new(account);
    let path = data_dir.account_database(&id)?;
    let store = Store::open(&path)
        .with_context(|| format!("could not open the store at {}", path.display()))?;
    store.upsert_account(&Account {
        id,
        email: account.to_owned(),
        display_name: None,
    })?;
    Ok((store, path))
}

/// A token cancelled by Ctrl-C.
pub fn cancel_on_ctrl_c() -> CancellationToken {
    let cancel = CancellationToken::new();
    let on_ctrl_c = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            on_ctrl_c.cancel();
        }
    });
    cancel
}

/// One status line, such as `Inbox: 120 threads, 130 messages (4 unchanged)`.
pub fn progress_line(progress: &Progress) -> String {
    let phase = match progress.phase {
        Phase::Labels => "Labels",
        Phase::Inbox => "Inbox",
        Phase::Prefetch => "Bodies",
        Phase::Backfill => "Backfill",
        Phase::Done => "Done",
    };
    let mut line = format!(
        "{phase}{resumed}: {} labels, {} threads, {} messages",
        progress.labels,
        progress.threads,
        progress.messages,
        resumed = if progress.resumed { " (resumed)" } else { "" },
    );
    if progress.skipped_threads > 0 {
        line.push_str(&format!(" ({} unchanged)", progress.skipped_threads));
    }
    if progress.bodies > 0 {
        line.push_str(&format!(", {} bodies", progress.bodies));
    }
    if progress.deleted_threads > 0 {
        line.push_str(&format!(", {} gone", progress.deleted_threads));
    }
    if progress.retried_requests > 0 {
        line.push_str(&format!(
            ", {} retried ({} rate-limited)",
            progress.retried_requests, progress.rate_limited_requests
        ));
    }
    if let (Some(spent), Some(budget)) = (progress.spent_last_minute, progress.budget_per_minute) {
        line.push_str(&format!(", quota {spent}/{budget} a min"));
    }
    if let Some(wait) = progress.rate_limit_wait {
        line.push_str(&format!(
            " — waiting {}s for Gmail rate limit",
            wait.as_secs()
        ));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_the_progress_line() {
        let mut progress = Progress {
            phase: Phase::Inbox,
            labels: 12,
            threads: 120,
            messages: 130,
            ..Progress::default()
        };
        assert_eq!(
            progress_line(&progress),
            "Inbox: 12 labels, 120 threads, 130 messages"
        );

        progress.phase = Phase::Backfill;
        progress.resumed = true;
        progress.skipped_threads = 4;
        progress.bodies = 25;
        assert_eq!(
            progress_line(&progress),
            "Backfill (resumed): 12 labels, 120 threads, 130 messages (4 unchanged), 25 bodies"
        );

        progress.retried_requests = 7;
        progress.rate_limited_requests = 5;
        progress.budget_per_minute = Some(2_340);
        progress.spent_last_minute = Some(2_105);
        assert!(
            progress_line(&progress)
                .ends_with(", 25 bodies, 7 retried (5 rate-limited), quota 2105/2340 a min"),
            "{}",
            progress_line(&progress)
        );

        progress.rate_limit_wait = Some(std::time::Duration::from_secs(60));
        assert!(
            progress_line(&progress).ends_with("a min — waiting 60s for Gmail rate limit"),
            "{}",
            progress_line(&progress)
        );
    }
}
