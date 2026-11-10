//! Developer CLI for exercising Gmail, MIME, store and sync without the desktop UI.
//!
//! See the crate's `README.md` for setting up a Google OAuth client and for usage.

mod accounts;
mod cli;
mod labels;
mod session;
mod text;
mod threads;

use std::process::ExitCode;

use anyhow::bail;
use clap::Parser;
use time::UtcOffset;
use tracing_subscriber::EnvFilter;

use crate::cli::{Cli, Command};

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing();
    // Read the local UTC offset while this is the only thread: the `time` crate
    // refuses to once others are running.
    let offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("error: could not start the async runtime: {err}");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(run(cli, offset)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            if session::needs_sign_in(&err) {
                eprintln!("hint: run `openmail-cli login` to sign in again");
            }
            ExitCode::FAILURE
        }
    }
}

/// Logs to stderr, filtered by `RUST_LOG` (warnings and errors by default).
fn init_tracing() {
    let filter = EnvFilter::builder()
        .with_default_directive(tracing::Level::WARN.into())
        .from_env_lossy();
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

async fn run(cli: Cli, offset: UtcOffset) -> anyhow::Result<()> {
    tracing::debug!(?cli, "starting");
    let account = cli.account.as_deref();
    match cli.command {
        Command::Login => session::login().await,
        Command::Logout => session::logout(account).await,
        Command::Labels => labels::run(&session::gmail_client(account)?).await,
        Command::Threads {
            label,
            limit,
            query,
        } => {
            threads::run(
                &session::gmail_client(account)?,
                &label,
                limit,
                query,
                offset,
            )
            .await
        }
        Command::Show { .. } => {
            let _client = session::gmail_client(account)?;
            bail!("not implemented yet")
        }
    }
}
