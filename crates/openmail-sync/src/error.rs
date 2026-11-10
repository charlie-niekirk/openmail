//! Sync errors.

use std::time::Duration;

use openmail_gmail::api::{self, QuotaKind};

/// A sync result.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Why a sync stopped.
///
/// Progress up to the last completed page is saved, so running the sync again
/// continues from there.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A Gmail API call failed (after the client's own retries).
    #[error("Gmail request failed")]
    Gmail(#[from] api::Error),

    /// The local store failed.
    #[error("local store failed")]
    Store(#[from] openmail_store::Error),

    /// The sync was cancelled through its `CancellationToken`.
    #[error("sync was cancelled")]
    Cancelled,

    /// Incremental sync needs the initial sync to have finished first.
    #[error("the initial sync has not finished")]
    InitialSyncIncomplete,
}

impl Error {
    /// If this is Gmail's short-window rate limit, `Some` with how long Gmail
    /// asked to wait (if it said).
    pub fn rate_limit_retry_after(&self) -> Option<Option<Duration>> {
        match self {
            Self::Gmail(api::Error::Quota {
                kind: QuotaKind::RateLimit,
                retry_after,
                ..
            }) => Some(*retry_after),
            _ => None,
        }
    }

    /// Whether running the sync again later may succeed on its own: network
    /// failures, rate limits that outlasted the client's retries, and Gmail
    /// server errors. Sign-in problems, an exhausted daily quota, store errors
    /// and cancellation are not transient.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Gmail(err) => match err {
                api::Error::Http(_) => true,
                api::Error::Quota { kind, .. } => *kind == QuotaKind::RateLimit,
                api::Error::Api { status, .. } => status.is_server_error(),
                _ => false,
            },
            Self::Store(_) | Self::Cancelled | Self::InitialSyncIncomplete => false,
        }
    }
}
