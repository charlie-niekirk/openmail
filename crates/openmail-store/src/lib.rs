//! Local SQLite store with FTS5 search, migrations and the queries the UI and sync engine need.
//!
//! Each account has its own database (see [`DataDir`]), opened in WAL mode so the UI
//! can read while sync writes.

mod convert;
mod error;
mod image_senders;
mod labels;
mod mail;
mod migrations;
mod ops;
mod paths;
mod queries;
mod store;
mod sync_state;
mod sync_writes;

pub use error::{Error, Result};
pub use mail::{InlinePart, MessageBody};
pub use ops::{LabelDelta, PendingOp, PendingOpState};
pub use paths::{DATA_DIR_ENV, DataDir};
pub use queries::{
    LabelCounts, MessageDetail, ThreadCursor, ThreadDetail, ThreadPage, ThreadSummary, ThreadView,
};
pub use store::Store;
pub use sync_state::{QuotaBudget, SyncState};

#[cfg(test)]
mod tests;
