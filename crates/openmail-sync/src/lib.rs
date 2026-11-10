//! Sync engine, offline operations queue and the tokio-based `MailService` the UI talks to.
//!
//! [`InitialSync`] downloads a mailbox's labels and thread metadata into the
//! local store, resumably. [`HistorySync`] then keeps it up to date from
//! Gmail's `history.list`, and [`Scheduler`] runs both: the initial sync
//! until it is done, then a poll every so often. [`OpsQueue`] applies user
//! actions to the store at once and sends them to Gmail in the background.
//! [`MailService`] runs all of this on a tokio runtime of its own and is what
//! the UI talks to: commands through a [`MailHandle`], events through
//! [`MailEvents`].

mod error;
mod history;
mod initial;
mod ops;
mod scheduler;
mod service;

pub use error::{Error, Result};
pub use history::{Changes, HISTORY_PAGE_SIZE, HistorySync};
pub use initial::{Cutoff, InitialSync, InitialSyncConfig, Phase, Progress, SYNC_HEADERS};
pub use ops::{Flush, Op, OpEvent, OpId, OpsConfig, OpsQueue, QueuedOp, Target};
pub use scheduler::{
    Activity, Connectivity, Scheduler, SchedulerConfig, SchedulerHandle, SyncEvent,
};
pub use service::{MailEvent, MailEvents, MailHandle, MailService, MailServiceConfig, SyncStatus};
pub use tokio_util::sync::CancellationToken;

#[cfg(test)]
mod tests {
    use openmail_gmail::api::{self, QuotaKind};

    use super::*;

    #[test]
    fn transient_errors_are_classified() {
        let quota = |kind| {
            Error::from(api::Error::Quota {
                kind,
                message: String::new(),
                retry_after: None,
            })
        };
        assert!(quota(QuotaKind::RateLimit).is_transient());
        assert!(!quota(QuotaKind::Daily).is_transient());
        assert!(!Error::from(api::Error::Unauthorized).is_transient());
        assert!(!Error::Cancelled.is_transient());
        assert!(!Error::from(openmail_store::Error::NoDataDir).is_transient());
    }
}
