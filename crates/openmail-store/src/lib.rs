//! Local SQLite store with FTS5 search, migrations and the queries the UI and sync engine need.
//!
//! Each account has its own database (see [`DataDir`]), opened in WAL mode so the UI
//! can read while sync writes.

mod error;
mod paths;
mod store;

pub use error::{Error, Result};
pub use paths::{DATA_DIR_ENV, DataDir};
pub use store::Store;
