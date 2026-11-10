//! Sync progress.

use openmail_core::HistoryId;
use rusqlite::named_params;
use time::OffsetDateTime;

use crate::convert::{date_from_sql, date_to_sql, history_from_sql, history_to_sql};
use crate::{Result, Store};

/// How far the account has synced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncState {
    /// The history ID incremental sync continues from. `None` before the first sync.
    pub history_id: Option<HistoryId>,
    /// Where the initial backfill resumes. Opaque to the store.
    pub backfill_cursor: Option<String>,
    /// Whether the initial backfill has finished.
    pub backfill_complete: bool,
    /// When the last full sync finished.
    pub last_full_sync: Option<OffsetDateTime>,
}

impl Store {
    /// The account's sync progress.
    pub fn sync_state(&self) -> Result<SyncState> {
        let (history_id, backfill_cursor, backfill_complete, last_full_sync): (
            Option<i64>,
            Option<String>,
            bool,
            Option<i64>,
        ) = self.conn().query_row(
            "SELECT history_id, backfill_cursor, backfill_complete, last_full_sync
             FROM sync_state WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        Ok(SyncState {
            history_id: history_id.map(history_from_sql).transpose()?,
            backfill_cursor,
            backfill_complete,
            last_full_sync: last_full_sync.map(date_from_sql).transpose()?,
        })
    }

    /// Replaces the account's sync progress.
    pub fn set_sync_state(&self, state: &SyncState) -> Result<()> {
        self.conn().execute(
            "UPDATE sync_state SET history_id = :history_id, backfill_cursor = :cursor,
                 backfill_complete = :complete, last_full_sync = :last_full_sync
             WHERE id = 1",
            named_params! {
                ":history_id": state.history_id.map(history_to_sql).transpose()?,
                ":cursor": state.backfill_cursor,
                ":complete": state.backfill_complete,
                ":last_full_sync": state.last_full_sync.map(date_to_sql).transpose()?,
            },
        )?;
        Ok(())
    }
}
