//! Serde models mirroring Gmail's JSON resources.

use openmail_core::HistoryId;
use serde::Deserialize;

/// The signed-in user's mailbox profile (`users.getProfile`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    pub email_address: String,
    #[serde(default)]
    pub messages_total: u64,
    #[serde(default)]
    pub threads_total: u64,
    /// The mailbox's current history ID: where incremental sync starts.
    pub history_id: HistoryId,
}
