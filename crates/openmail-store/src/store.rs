//! Opening a database and running transactions.

use std::fs;
use std::path::Path;
use std::time::Duration;

use openmail_core::AccountId;
use rusqlite::Connection;

use crate::{DataDir, Error, Result, migrations};

/// How long a statement waits for another connection's lock before failing.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// An open account database.
///
/// Reads take `&self`. Writes take `&self` too and are atomic on their own; wrap
/// several in [`Store::transaction`] to make them atomic together.
#[derive(Debug)]
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens (creating if needed) the database for `account` under `data_dir`.
    pub fn open_account(data_dir: &DataDir, account: &AccountId) -> Result<Self> {
        Self::open(&data_dir.account_database(account)?)
    }

    /// Opens (creating if needed) the database at `path`, creating its directory.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
            fs::create_dir_all(dir).map_err(|source| Error::CreateDir {
                path: dir.to_owned(),
                source,
            })?;
        }
        Self::setup(Connection::open(path)?)
    }

    /// Opens a private in-memory database, for tests.
    pub fn open_in_memory() -> Result<Self> {
        Self::setup(Connection::open_in_memory()?)
    }

    fn setup(mut conn: Connection) -> Result<Self> {
        configure(&conn)?;
        migrations::migrate(&mut conn)?;
        Ok(Self { conn })
    }

    /// The underlying connection, for this crate's queries.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "used by the queries added next")
    )]
    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Runs `f` atomically: everything it writes is committed if it returns `Ok`
    /// and rolled back if it returns `Err` or panics.
    ///
    /// Transactions nest: inside another transaction this uses a savepoint, so
    /// write methods (which are transactions themselves) compose.
    pub fn transaction<T, E>(&self, f: impl FnOnce(&Self) -> Result<T, E>) -> Result<T, E>
    where
        E: From<Error>,
    {
        let guard = TxGuard::begin(&self.conn).map_err(Error::from)?;
        let value = f(self)?;
        guard.commit().map_err(Error::from)?;
        Ok(value)
    }
}

/// Applies the connection settings every Openmail database uses.
fn configure(conn: &Connection) -> Result<()> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    // WAL lets the UI read while sync writes. In-memory databases report "memory".
    conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get::<_, String>(0))?;
    // NORMAL is durable in WAL mode except across power loss, which only costs a
    // re-sync of the last changes.
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", true)?;
    Ok(())
}

/// An open transaction or savepoint that rolls back unless committed.
struct TxGuard<'c> {
    conn: &'c Connection,
    /// Whether this is a savepoint inside an outer transaction.
    nested: bool,
    done: bool,
}

impl<'c> TxGuard<'c> {
    const SAVEPOINT: &str = "openmail_store";

    fn begin(conn: &'c Connection) -> rusqlite::Result<Self> {
        let nested = !conn.is_autocommit();
        if nested {
            conn.execute_batch(&format!("SAVEPOINT {}", Self::SAVEPOINT))?;
        } else {
            // IMMEDIATE takes the write lock up front, so a transaction that reads
            // before it writes cannot fail to upgrade its lock half-way through.
            conn.execute_batch("BEGIN IMMEDIATE")?;
        }
        Ok(Self {
            conn,
            nested,
            done: false,
        })
    }

    fn commit(mut self) -> rusqlite::Result<()> {
        self.done = true;
        if self.nested {
            self.conn
                .execute_batch(&format!("RELEASE {}", Self::SAVEPOINT))
        } else {
            self.conn.execute_batch("COMMIT")
        }
    }
}

impl Drop for TxGuard<'_> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        // Errors are ignored: there is nothing more to undo if the rollback fails.
        let sql = if self.nested {
            format!("ROLLBACK TO {sp}; RELEASE {sp}", sp = Self::SAVEPOINT)
        } else {
            "ROLLBACK".to_owned()
        };
        let _ = self.conn.execute_batch(&sql);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pragma(store: &Store, name: &str) -> String {
        store
            .conn()
            .pragma_query_value(None, name, |row| row.get::<_, rusqlite::types::Value>(0))
            .map(|value| match value {
                rusqlite::types::Value::Integer(n) => n.to_string(),
                rusqlite::types::Value::Text(s) => s,
                other => format!("{other:?}"),
            })
            .unwrap()
    }

    #[test]
    fn file_database_uses_wal_and_settings() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::at(dir.path());
        let account = AccountId::new("ada@example.com");
        let store = Store::open_account(&data, &account).unwrap();

        assert!(data.account_database(&account).unwrap().exists());
        assert_eq!(pragma(&store, "journal_mode"), "wal");
        assert_eq!(pragma(&store, "foreign_keys"), "1");
        // NORMAL is 1.
        assert_eq!(pragma(&store, "synchronous"), "1");
        assert_eq!(pragma(&store, "busy_timeout"), "5000");
    }

    #[test]
    fn reopening_keeps_wal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("mail.db");
        drop(Store::open(&path).unwrap());
        let store = Store::open(&path).unwrap();
        assert_eq!(pragma(&store, "journal_mode"), "wal");
    }

    #[test]
    fn transactions_commit_roll_back_and_nest() {
        let store = Store::open_in_memory().unwrap();
        store
            .conn()
            .execute_batch("CREATE TABLE t (v INTEGER)")
            .unwrap();
        let count = |store: &Store| -> i64 {
            store
                .conn()
                .query_row("SELECT count(*) FROM t", [], |row| row.get(0))
                .unwrap()
        };

        store
            .transaction(|s| {
                s.conn().execute("INSERT INTO t VALUES (1)", [])?;
                // A failing inner transaction rolls back only its own writes.
                let inner: Result<()> = s.transaction(|s| {
                    s.conn().execute("INSERT INTO t VALUES (2)", [])?;
                    Err(Error::Corrupt("boom".to_owned()))
                });
                assert!(inner.is_err());
                Ok::<_, Error>(())
            })
            .unwrap();
        assert_eq!(count(&store), 1);

        let outer: Result<()> = store.transaction(|s| {
            s.conn().execute("INSERT INTO t VALUES (3)", [])?;
            Err(Error::Corrupt("boom".to_owned()))
        });
        assert!(outer.is_err());
        assert_eq!(count(&store), 1);
        assert!(store.conn().is_autocommit());
    }
}
