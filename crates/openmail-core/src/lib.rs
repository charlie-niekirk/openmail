//! Domain types shared by every Openmail crate: accounts, labels, threads, messages, drafts and errors.
//!
//! The types follow Gmail's model: labels rather than folders, threads of messages,
//! and history IDs for delta sync. This crate is pure data and does no I/O.

mod account;
mod address;
mod error;
mod id;
mod label;
mod message;

pub use account::Account;
pub use address::Address;
pub use error::{Error, Result};
pub use id::{AccountId, AttachmentId, DraftId, HistoryId, LabelId, MessageId, ThreadId};
pub use label::{
    LABEL_PATH_SEPARATOR, Label, LabelColor, LabelListVisibility, LabelPath, LabelType,
    MessageListVisibility, SystemLabel,
};
pub use message::{Attachment, Draft, Message, MessageHeaders, Thread};
