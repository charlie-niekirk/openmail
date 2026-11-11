//! Starting the mail service for the account to open.
//!
//! Until onboarding (PR 3.2) signs accounts in from the app, the account is
//! one the dev CLI signed in (`openmail-cli login`) and synced
//! (`openmail-cli sync`): `OPENMAIL_ACCOUNT` if set, otherwise the only
//! account with a database in the data directory.

use std::fs;
use std::io;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context as _;
use openmail_core::AccountId;
use openmail_gmail::GmailClient;
use openmail_gmail::api::{MinuteBudget, QuotaPacing};
use openmail_gmail::auth::{
    AuthError, CLIENT_ID_ENV, KeyringTokenStore, OAuthClient, OAuthConfig, RefreshingTokenProvider,
    TokenStore,
};
use openmail_store::DataDir;
use openmail_sync::{MailService, MailServiceConfig};
use openmail_ui::Connection;

/// The environment variable that picks the account to open.
pub const ACCOUNT_ENV: &str = "OPENMAIL_ACCOUNT";

/// Starts the mail service for the account to open, or says why there is
/// none. Blocks (store, keychain), so it runs off the main thread.
pub fn connect() -> Connection {
    try_connect().unwrap_or_else(|err| Connection::Failed {
        error: format!("{err:#}"),
    })
}

fn try_connect() -> anyhow::Result<Connection> {
    let data_dir = DataDir::from_env_or_platform().context("could not find the data directory")?;
    let requested = std::env::var(ACCOUNT_ENV).ok().filter(|a| !a.is_empty());
    let account = match choose_account(requested, &synced_accounts(data_dir.root())?) {
        Choice::One(account) => account,
        Choice::None => {
            return Ok(signed_out(
                "No account is synced yet. Sign in with `openmail-cli login`, then run \
                 `openmail-cli sync`.",
            ));
        }
        Choice::Several(accounts) => {
            return Ok(signed_out(format!(
                "Several accounts are synced ({}). Choose one with {ACCOUNT_ENV}.",
                accounts.join(", ")
            )));
        }
    };

    let config = match OAuthConfig::from_env() {
        Ok(config) => config,
        Err(AuthError::MissingClientId) => {
            return Ok(signed_out(format!(
                "{CLIENT_ID_ENV} is not set. See crates/openmail-cli/README.md."
            )));
        }
        Err(err) => return Err(err.into()),
    };
    let tokens: Arc<dyn TokenStore> = Arc::new(KeyringTokenStore::new());
    if tokens.load(&account)?.is_none() {
        return Ok(signed_out(format!(
            "{account} is not signed in. Run `openmail-cli login`."
        )));
    }

    let provider =
        RefreshingTokenProvider::new(OAuthClient::new(&config)?, tokens, account.clone());
    let client = GmailClient::builder(Arc::new(provider))
        .quota_pacing(QuotaPacing::Budget(MinuteBudget::from_env()))
        .build()?;
    let database = data_dir.account_database(&AccountId::new(account.as_str()))?;
    let (service, events) = MailService::start(client, MailServiceConfig::new(&database))
        .with_context(|| format!("could not open {}", database.display()))?;
    tracing::info!(%account, "started the mail service");
    Ok(Connection::Started {
        account,
        service,
        events,
    })
}

fn signed_out(reason: impl Into<String>) -> Connection {
    Connection::SignedOut {
        reason: reason.into(),
    }
}

/// Which account to open.
#[derive(Debug, PartialEq, Eq)]
enum Choice {
    None,
    One(String),
    Several(Vec<String>),
}

/// The `requested` account, else the only synced one.
fn choose_account(requested: Option<String>, synced: &[String]) -> Choice {
    if let Some(account) = requested {
        return Choice::One(account);
    }
    match synced {
        [] => Choice::None,
        [account] => Choice::One(account.clone()),
        several => Choice::Several(several.to_vec()),
    }
}

/// The accounts with a database under the data directory `root`, sorted.
fn synced_accounts(root: &Path) -> io::Result<Vec<String>> {
    let entries = match fs::read_dir(root.join("accounts")) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let mut accounts = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.path().join(DataDir::DATABASE_FILE).is_file()
            && let Some(name) = entry.file_name().to_str()
        {
            accounts.push(name.to_owned());
        }
    }
    accounts.sort();
    Ok(accounts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(accounts: &[&str]) -> Vec<String> {
        accounts.iter().map(|&a| a.to_owned()).collect()
    }

    #[test]
    fn opens_the_requested_or_only_account() {
        let one = owned(&["ada@example.com"]);
        let two = owned(&["ada@example.com", "grace@example.com"]);
        assert_eq!(choose_account(None, &[]), Choice::None);
        assert_eq!(
            choose_account(None, &one),
            Choice::One("ada@example.com".to_owned())
        );
        assert_eq!(choose_account(None, &two), Choice::Several(two.clone()));
        assert_eq!(
            choose_account(Some("grace@example.com".to_owned()), &two),
            Choice::One("grace@example.com".to_owned())
        );
    }

    #[test]
    fn lists_accounts_that_have_a_database() {
        let root = tempfile::tempdir().unwrap();
        assert!(synced_accounts(root.path()).unwrap().is_empty());

        let accounts = root.path().join("accounts");
        for account in ["grace@example.com", "ada@example.com"] {
            let dir = accounts.join(account);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(DataDir::DATABASE_FILE), b"").unwrap();
        }
        fs::create_dir_all(accounts.join("empty@example.com")).unwrap();
        fs::write(accounts.join("stray-file"), b"").unwrap();

        assert_eq!(
            synced_accounts(root.path()).unwrap(),
            owned(&["ada@example.com", "grace@example.com"])
        );
    }
}
