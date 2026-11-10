//! Store errors.

use std::path::PathBuf;

/// A store result.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Why a store operation failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// SQLite reported an error.
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// Schema migration `version` failed.
    #[error("database migration {version} failed: {source}")]
    Migration {
        version: u32,
        #[source]
        source: rusqlite::Error,
    },

    /// The database was written by a newer Openmail, with a schema this one does
    /// not know.
    #[error("database schema version {found} is newer than this build supports ({supported})")]
    SchemaTooNew { found: i64, supported: usize },

    /// The database's directory could not be created.
    #[error("could not create {}: {source}", path.display())]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// No home directory was found to keep the data directory in.
    #[error("could not find a home directory to keep Openmail's data in")]
    NoDataDir,

    /// An account ID cannot be used as a directory name.
    #[error("account ID {0:?} cannot be used as a directory name")]
    InvalidAccountId(String),

    /// A row the operation needs is not stored, such as the message whose labels
    /// are being set.
    #[error("{0} is not in the store")]
    NotFound(String),

    /// A value does not fit the database's column, such as a `u64` above `i64::MAX`.
    #[error("{0} is out of range for the database")]
    OutOfRange(&'static str),

    /// A stored value could not be read back, such as an unknown enum string.
    #[error("invalid value in the database: {0}")]
    Corrupt(String),
}
