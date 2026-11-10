//! Local SQLite store with FTS5 search, migrations and the queries the UI and sync engine need.
//!
//! Each account has its own database (see [`DataDir`]), opened in WAL mode so the UI
//! can read while sync writes.

mod convert;
mod error;
mod labels;
mod mail;
mod migrations;
mod paths;
mod store;
mod sync_state;

pub use error::{Error, Result};
pub use mail::MessageBody;
pub use paths::{DATA_DIR_ENV, DataDir};
pub use store::Store;
pub use sync_state::SyncState;
