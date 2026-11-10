//! Where account databases live on disk.
//!
//! Each account has its own directory, and its own database, under the data
//! directory: `<data dir>/accounts/<account id>/mail.db`. The data directory is the
//! platform's (for example `~/Library/Application Support/Openmail` on macOS), unless
//! `OPENMAIL_DATA_DIR` names another one or a caller (such as a test) picks one.

use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use openmail_core::AccountId;

use crate::{Error, Result};

/// The environment variable that overrides the platform data directory.
pub const DATA_DIR_ENV: &str = "OPENMAIL_DATA_DIR";

/// The directory Openmail keeps its local data in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataDir {
    root: PathBuf,
}

impl DataDir {
    /// The database file name inside an account's directory.
    pub const DATABASE_FILE: &str = "mail.db";

    /// A data directory at `root`, for tests and custom setups.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// `OPENMAIL_DATA_DIR` if it is set, otherwise the platform data directory.
    pub fn from_env_or_platform() -> Result<Self> {
        match std::env::var_os(DATA_DIR_ENV) {
            Some(dir) if !dir.is_empty() => Ok(Self::at(dir)),
            _ => Self::platform(),
        }
    }

    /// The platform data directory for Openmail.
    pub fn platform() -> Result<Self> {
        ProjectDirs::from("", "", "Openmail")
            .map(|dirs| Self::at(dirs.data_dir()))
            .ok_or(Error::NoDataDir)
    }

    /// The data directory itself.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The directory that holds an account's data.
    pub fn account_dir(&self, account: &AccountId) -> Result<PathBuf> {
        let id = account.as_str();
        if !is_safe_dir_name(id) {
            return Err(Error::InvalidAccountId(id.to_owned()));
        }
        Ok(self.root.join("accounts").join(id))
    }

    /// The path of an account's database.
    pub fn account_database(&self, account: &AccountId) -> Result<PathBuf> {
        Ok(self.account_dir(account)?.join(Self::DATABASE_FILE))
    }
}

/// Whether `name` is usable as a single directory name on every platform.
fn is_safe_dir_name(name: &str) -> bool {
    const RESERVED: &[char] = &['/', '\\', ':', '*', '?', '"', '<', '>', '|'];
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.ends_with(['.', ' '])
        && !name
            .chars()
            .any(|c| c.is_control() || RESERVED.contains(&c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_database_is_under_accounts() {
        let dir = DataDir::at("/data");
        let path = dir
            .account_database(&AccountId::new("ada@example.com"))
            .unwrap();
        assert_eq!(
            path,
            Path::new("/data")
                .join("accounts")
                .join("ada@example.com")
                .join("mail.db")
        );
    }

    #[test]
    fn rejects_account_ids_that_escape_the_directory() {
        let dir = DataDir::at("/data");
        for id in ["", ".", "..", "a/b", "a\\b", "c:", "trailing.", "nul\0"] {
            assert!(
                matches!(
                    dir.account_dir(&AccountId::new(id)),
                    Err(Error::InvalidAccountId(_))
                ),
                "{id:?} should be rejected"
            );
        }
    }
}
