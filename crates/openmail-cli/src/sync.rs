//! `sync`: the initial sync into the account's local store.

use std::io::Write as _;

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
    if store.sync_state()?.backfill_complete {
        println!(
            "The initial sync of {account} is already complete ({}).",
            path.display()
        );
        return Ok(());
    }

    let cancel = CancellationToken::new();
    let on_ctrl_c = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            on_ctrl_c.cancel();
        }
    });

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
    }
}
