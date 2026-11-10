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

/// A per-minute Gmail quota budget learned from rate limiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaBudget {
    /// Units allowed per minute.
    pub budget: u32,
    /// The project's per-minute quota it was learned under.
    pub units_per_minute: u32,
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

    /// The quota budget saved for the account, if any.
    pub fn quota_budget(&self) -> Result<Option<QuotaBudget>> {
        let (budget, basis): (Option<u32>, Option<u32>) = self.conn().query_row(
            "SELECT quota_budget, quota_budget_basis FROM sync_state WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(budget
            .zip(basis)
            .map(|(budget, units_per_minute)| QuotaBudget {
                budget,
                units_per_minute,
            }))
    }

    /// Saves (or with `None`, forgets) the account's quota budget. It is kept
    /// apart from [`SyncState`], so resetting the sync keeps it.
    pub fn set_quota_budget(&self, budget: Option<QuotaBudget>) -> Result<()> {
        self.conn().execute(
            "UPDATE sync_state SET quota_budget = ?1, quota_budget_basis = ?2 WHERE id = 1",
            rusqlite::params![budget.map(|b| b.budget), budget.map(|b| b.units_per_minute)],
        )?;
        Ok(())
    }
}
