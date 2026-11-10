//! Gmail API calls.

use super::{GmailClient, Profile, Result};

impl GmailClient {
    /// `users.getProfile`: the address, totals and current history ID.
    pub async fn get_profile(&self) -> Result<Profile> {
        self.get_json(&["profile"], &[]).await
    }
}
