//! Schema migrations, embedded from `migrations/*.sql`.
//!
//! The schema version is SQLite's `user_version`: the number of migrations applied.
//! Migrations are append-only: never edit one that has been released; add a new
//! file to [`MIGRATIONS`].

use rusqlite::{Connection, TransactionBehavior};

use crate::{Error, Result};

/// Every migration, in order. Migration `n` (1-based) moves the schema from
/// version `n - 1` to `n`.
const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/0001_initial.sql"),
    include_str!("../migrations/0002_resync_seen.sql"),
    include_str!("../migrations/0003_quota_budget.sql"),
    include_str!("../migrations/0004_ops_queue.sql"),
];

/// Brings the database's schema up to date, all in one transaction.
pub(crate) fn migrate(conn: &mut Connection) -> Result<()> {
    // IMMEDIATE: two processes opening the same new database migrate one at a time.
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let found: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let current = usize::try_from(found)
        .ok()
        .filter(|&version| version <= MIGRATIONS.len())
        .ok_or(Error::SchemaTooNew {
            found,
            supported: MIGRATIONS.len(),
        })?;
    if current == MIGRATIONS.len() {
        return Ok(());
    }

    for (version, sql) in (1u32..).zip(MIGRATIONS).skip(current) {
        let apply = || -> rusqlite::Result<()> {
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", version)
        };
        apply().map_err(|source| Error::Migration { version, source })?;
    }

    let violations: i64 =
        tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    if violations > 0 {
        return Err(Error::Corrupt(format!(
            "{violations} foreign key violations after migrating"
        )));
    }
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_version(conn: &Connection) -> i64 {
        conn.pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn migrations_apply_from_empty() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        migrate(&mut conn).unwrap();
        assert_eq!(
            user_version(&conn),
            i64::try_from(MIGRATIONS.len()).unwrap()
        );

        let mut stmt = conn
            .prepare(
                "SELECT name FROM sqlite_schema WHERE type IN ('table', 'view') \
                 AND name NOT LIKE 'messages_fts_%' ORDER BY name",
            )
            .unwrap();
        let names: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            names,
            [
                "accounts",
                "attachments",
                "bodies",
                "drafts",
                "labels",
                "message_labels",
                "messages",
                "messages_fts",
                "pending_ops",
                "sqlite_sequence",
                "sync_seen_threads",
                "sync_state",
                "thread_labels",
                "threads",
            ]
        );
        drop(stmt);

        // Migrating again is a no-op.
        migrate(&mut conn).unwrap();
        assert_eq!(
            user_version(&conn),
            i64::try_from(MIGRATIONS.len()).unwrap()
        );
    }

    #[test]
    fn refuses_a_newer_schema() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "user_version", 999).unwrap();
        assert!(matches!(
            migrate(&mut conn),
            Err(Error::SchemaTooNew { found: 999, .. })
        ));
    }

    #[test]
    fn a_failed_migration_rolls_back() {
        let mut conn = Connection::open_in_memory().unwrap();
        // A table the first migration also creates makes it fail part-way.
        conn.execute_batch("CREATE TABLE threads (x)").unwrap();
        assert!(matches!(
            migrate(&mut conn),
            Err(Error::Migration { version: 1, .. })
        ));
        assert_eq!(user_version(&conn), 0);
        let accounts: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name = 'accounts'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(accounts, 0);
    }
}
