//! Sync engine, offline operations queue and the tokio-based `MailService` the UI talks to.
//!
//! [`InitialSync`] downloads a mailbox's labels and thread metadata into the
//! local store, resumably.

mod error;
mod initial;

pub use error::{Error, Result};
pub use initial::{Cutoff, InitialSync, InitialSyncConfig, Phase, Progress, SYNC_HEADERS};
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
