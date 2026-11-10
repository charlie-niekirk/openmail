//! Gmail API calls that change the mailbox.

use openmail_core::{LabelId, MessageId, ThreadId};
use reqwest::Method;
use serde::Serialize;

use super::{ApiLabel, ApiMessage, ApiThread, GmailClient, LabelRequest, ModifyLabels, Result};

/// The most message IDs `messages.batchModify` accepts in one call.
pub const MAX_BATCH_MODIFY_IDS: usize = 1000;

/// No request body.
const NO_BODY: Option<&()> = None;

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

    /// `messages.trash`: moves a message to Trash.
    pub async fn trash_message(&self, id: &MessageId) -> Result<ApiMessage> {
        self.call_json(Method::POST, &["messages", id.as_str(), "trash"], NO_BODY)
            .await
    }

    /// `messages.untrash`: takes a message out of Trash.
    pub async fn untrash_message(&self, id: &MessageId) -> Result<ApiMessage> {
        self.call_json(Method::POST, &["messages", id.as_str(), "untrash"], NO_BODY)
            .await
    }

    /// `messages.delete`: deletes a message immediately and permanently,
    /// skipping Trash.
    pub async fn delete_message(&self, id: &MessageId) -> Result<()> {
        self.call_empty(Method::DELETE, &["messages", id.as_str()], NO_BODY)
            .await
    }

    /// `threads.trash`: moves every message in a thread to Trash.
    pub async fn trash_thread(&self, id: &ThreadId) -> Result<ApiThread> {
        self.call_json(Method::POST, &["threads", id.as_str(), "trash"], NO_BODY)
            .await
    }

    /// `threads.untrash`: takes every message in a thread out of Trash.
    pub async fn untrash_thread(&self, id: &ThreadId) -> Result<ApiThread> {
        self.call_json(Method::POST, &["threads", id.as_str(), "untrash"], NO_BODY)
            .await
    }

    /// `threads.delete`: deletes a thread and its messages immediately and
    /// permanently, skipping Trash.
    pub async fn delete_thread(&self, id: &ThreadId) -> Result<()> {
        self.call_empty(Method::DELETE, &["threads", id.as_str()], NO_BODY)
            .await
    }

    /// `labels.create`: creates a user label. `label` needs a name.
    pub async fn create_label(&self, label: &LabelRequest) -> Result<ApiLabel> {
        self.call_json(Method::POST, &["labels"], Some(label)).await
    }

    /// `labels.patch`: changes the fields set in `changes` and leaves the
    /// rest as they are.
    pub async fn patch_label(&self, id: &LabelId, changes: &LabelRequest) -> Result<ApiLabel> {
        self.call_json(Method::PATCH, &["labels", id.as_str()], Some(changes))
            .await
    }

    /// `labels.delete`: deletes a user label and removes it from every
    /// message and thread. The messages themselves are kept.
    pub async fn delete_label(&self, id: &LabelId) -> Result<()> {
        self.call_empty(Method::DELETE, &["labels", id.as_str()], NO_BODY)
            .await
    }
}
