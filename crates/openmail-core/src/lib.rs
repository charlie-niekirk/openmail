//! Domain types shared by every Openmail crate: accounts, labels, threads, messages, drafts and errors.
//!
//! The types follow Gmail's model: labels rather than folders, threads of messages,
//! and history IDs for delta sync. This crate is pure data and does no I/O.

mod error;
mod id;

pub use error::{Error, Result};
pub use id::{AccountId, AttachmentId, DraftId, HistoryId, LabelId, MessageId, ThreadId};
