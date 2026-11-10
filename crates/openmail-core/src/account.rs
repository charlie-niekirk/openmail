//! Signed-in Google accounts.

use serde::{Deserialize, Serialize};

use crate::AccountId;

/// A Google account signed in to Openmail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub id: AccountId,
    /// The account's primary address, such as `ada@gmail.com`.
    pub email: String,
    /// The account holder's name, if known.
    pub display_name: Option<String>,
}
