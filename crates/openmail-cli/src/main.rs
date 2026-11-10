//! Developer CLI for exercising Gmail, MIME, store and sync without the desktop UI.
//!
//! See the crate's `README.md` for setting up a Google OAuth client and for usage.

mod accounts;
mod cli;
mod session;

use std::process::ExitCode;

use anyhow::bail;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::cli::{Cli, Command};

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing();

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

    match runtime.block_on(run(cli)) {
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

async fn run(cli: Cli) -> anyhow::Result<()> {
    tracing::debug!(?cli, "starting");
    let account = cli.account.as_deref();
    match cli.command {
        Command::Login => session::login().await,
        Command::Logout => session::logout(account).await,
        Command::Labels | Command::Threads { .. } | Command::Show { .. } => {
            let _client = session::gmail_client(account)?;
            bail!("not implemented yet")
        }
    }
}
