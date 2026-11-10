//! Which accounts the CLI has signed in.
//!
//! The keychain holds one refresh token per account but cannot list them, so the
//! CLI keeps the addresses (never the tokens) in a small file in the user's config
//! directory, one per line. The store (PR 2.1) will own the real accounts table.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use directories::ProjectDirs;

/// The signed-in accounts file.
#[derive(Debug, Clone)]
pub struct AccountList {
    path: PathBuf,
}

impl AccountList {
    /// The file name inside the config directory.
    const FILE_NAME: &str = "cli-accounts";

    /// The list in the platform's config directory for Openmail.
    pub fn open_default() -> anyhow::Result<Self> {
        let dirs = ProjectDirs::from("", "", "Openmail")
            .context("could not find a home directory to keep the account list in")?;
        Ok(Self::at(dirs.config_dir().join(Self::FILE_NAME)))
    }

    /// The list kept at `path`.
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Where the list is kept.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The signed-in accounts, in the order they signed in.
    pub fn load(&self) -> anyhow::Result<Vec<String>> {
        match fs::read_to_string(&self.path) {
            Ok(contents) => Ok(parse(&contents)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(err) => Err(err).with_context(|| format!("could not read {}", self.path.display())),
        }
    }

    /// Adds `email` if it is not already listed.
    pub fn add(&self, email: &str) -> anyhow::Result<()> {
        let mut accounts = self.load()?;
        if !contains(&accounts, email) {
            accounts.push(email.to_owned());
            self.save(&accounts)?;
        }
        Ok(())
    }

    /// Removes `email`, returning whether it was listed.
    pub fn remove(&self, email: &str) -> anyhow::Result<bool> {
        let mut accounts = self.load()?;
        let before = accounts.len();
        accounts.retain(|a| !a.eq_ignore_ascii_case(email));
        let removed = accounts.len() != before;
        if removed {
            self.save(&accounts)?;
        }
        Ok(removed)
    }

    fn save(&self, accounts: &[String]) -> anyhow::Result<()> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)
                .with_context(|| format!("could not create {}", dir.display()))?;
        }
        fs::write(&self.path, serialize(accounts))
            .with_context(|| format!("could not write {}", self.path.display()))
    }
}

/// Reads the file format: one address per line, blank lines ignored, duplicates
/// (ignoring ASCII case) dropped.
fn parse(contents: &str) -> Vec<String> {
    let mut accounts: Vec<String> = Vec::new();
    for line in contents.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if !contains(&accounts, line) {
            accounts.push(line.to_owned());
        }
    }
    accounts
}

fn serialize(accounts: &[String]) -> String {
    accounts.iter().map(|a| format!("{a}\n")).collect()
}

fn contains(accounts: &[String], email: &str) -> bool {
    accounts.iter().any(|a| a.eq_ignore_ascii_case(email))
}

/// Why no account could be picked.
#[derive(Debug, PartialEq, Eq)]
pub enum SelectError {
    /// Nobody has signed in.
    NoAccounts,
    /// Several accounts are signed in and none was asked for.
    Ambiguous(Vec<String>),
    /// The requested account is not signed in.
    Unknown {
        requested: String,
        accounts: Vec<String>,
    },
}

impl fmt::Display for SelectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoAccounts => f.write_str("no account is signed in; run `openmail-cli login`"),
            Self::Ambiguous(accounts) => write!(
                f,
                "several accounts are signed in; pick one with --account: {}",
                accounts.join(", ")
            ),
            Self::Unknown {
                requested,
                accounts,
            } if accounts.is_empty() => {
                write!(f, "{requested} is not signed in; run `openmail-cli login`")
            }
            Self::Unknown {
                requested,
                accounts,
            } => write!(
                f,
                "{requested} is not signed in (signed in: {}); run `openmail-cli login` to add it",
                accounts.join(", ")
            ),
        }
    }
}

impl std::error::Error for SelectError {}

/// Picks the account to use: the `requested` one if it is signed in (ignoring
/// ASCII case), otherwise the only signed-in account.
pub fn select(accounts: &[String], requested: Option<&str>) -> Result<String, SelectError> {
    match requested {
        Some(requested) => accounts
            .iter()
            .find(|a| a.eq_ignore_ascii_case(requested))
            .cloned()
            .ok_or_else(|| SelectError::Unknown {
                requested: requested.to_owned(),
                accounts: accounts.to_vec(),
            }),
        None => match accounts {
            [] => Err(SelectError::NoAccounts),
            [only] => Ok(only.clone()),
            many => Err(SelectError::Ambiguous(many.to_vec())),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accounts(list: &[&str]) -> Vec<String> {
        list.iter().map(|&a| a.to_owned()).collect()
    }

    #[test]
    fn file_format_round_trips() {
        let list = accounts(&["ada@example.com", "grace@example.com"]);
        assert_eq!(serialize(&list), "ada@example.com\ngrace@example.com\n");
        assert_eq!(parse(&serialize(&list)), list);
    }

    #[test]
    fn parse_skips_blanks_and_duplicates() {
        assert_eq!(
            parse("  ada@example.com \n\n\r\nADA@example.com\ngrace@example.com\r\n"),
            accounts(&["ada@example.com", "grace@example.com"])
        );
        assert!(parse("").is_empty());
    }

    #[test]
    fn defaults_to_the_only_account() {
        let list = accounts(&["ada@example.com"]);
        assert_eq!(select(&list, None).unwrap(), "ada@example.com");
    }

    #[test]
    fn requested_account_matches_ignoring_case() {
        let list = accounts(&["ada@example.com", "grace@example.com"]);
        assert_eq!(
            select(&list, Some("Grace@Example.com")).unwrap(),
            "grace@example.com"
        );
    }

    #[test]
    fn no_accounts_asks_to_log_in() {
        let err = select(&[], None).unwrap_err();
        assert_eq!(err, SelectError::NoAccounts);
        assert!(err.to_string().contains("openmail-cli login"));
    }

    #[test]
    fn several_accounts_need_an_explicit_choice() {
        let list = accounts(&["ada@example.com", "grace@example.com"]);
        let err = select(&list, None).unwrap_err();
        assert_eq!(
            err.to_string(),
            "several accounts are signed in; pick one with --account: \
             ada@example.com, grace@example.com"
        );
    }

    #[test]
    fn unknown_account_lists_the_signed_in_ones() {
        let list = accounts(&["ada@example.com"]);
        let err = select(&list, Some("grace@example.com")).unwrap_err();
        assert_eq!(
            err.to_string(),
            "grace@example.com is not signed in (signed in: ada@example.com); \
             run `openmail-cli login` to add it"
        );

        let err = select(&[], Some("grace@example.com")).unwrap_err();
        assert_eq!(
            err.to_string(),
            "grace@example.com is not signed in; run `openmail-cli login`"
        );
    }
}
