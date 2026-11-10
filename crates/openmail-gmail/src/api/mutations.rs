//! Gmail API calls that change the mailbox.

use openmail_core::{MessageId, ThreadId};
use reqwest::Method;
use serde::Serialize;

use super::{ApiMessage, ApiThread, GmailClient, ModifyLabels, Result};

/// The most message IDs `messages.batchModify` accepts in one call.
pub const MAX_BATCH_MODIFY_IDS: usize = 1000;

/// The `messages.batchModify` body.
#[derive(Serialize)]
struct BatchModifyBody<'a> {
    ids: &'a [MessageId],
    #[serde(flatten)]
    labels: &'a ModifyLabels,
}

impl GmailClient {
    /// `messages.modify`: adds and removes labels on one message. Returns the
    /// message's IDs and new labels.
    pub async fn modify_message(
        &self,
        id: &MessageId,
        labels: &ModifyLabels,
    ) -> Result<ApiMessage> {
        self.call_json(
            Method::POST,
            &["messages", id.as_str(), "modify"],
            Some(labels),
        )
        .await
    }

    /// `messages.batchModify`: adds and removes labels on many messages, in
    /// calls of at most [`MAX_BATCH_MODIFY_IDS`] IDs.
    ///
    /// Stops at the first failed call; the calls before it have been applied.
    /// The change is idempotent, so the caller can retry all of `ids`.
    pub async fn batch_modify_messages(
        &self,
        ids: &[MessageId],
        labels: &ModifyLabels,
    ) -> Result<()> {
        for chunk in ids.chunks(MAX_BATCH_MODIFY_IDS) {
            let body = BatchModifyBody { ids: chunk, labels };
            self.call_empty(Method::POST, &["messages", "batchModify"], Some(&body))
                .await?;
        }
        Ok(())
    }

    /// `threads.modify`: adds and removes labels on every message in a
    /// thread. Returns the thread's messages with their new labels.
    pub async fn modify_thread(&self, id: &ThreadId, labels: &ModifyLabels) -> Result<ApiThread> {
        self.call_json(
            Method::POST,
            &["threads", id.as_str(), "modify"],
            Some(labels),
        )
        .await
    }
}
