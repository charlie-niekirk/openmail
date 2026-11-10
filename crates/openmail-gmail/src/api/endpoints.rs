//! Gmail API calls.

use openmail_core::{AttachmentId, LabelId, MessageId, ThreadId};

use super::models::format_query;
use super::{
    ApiLabel, ApiMessage, ApiThread, Error, GmailClient, HistoryRequest, ListHistoryResponse,
    ListLabelsResponse, ListMessagesResponse, ListRequest, ListThreadsResponse, MessageFormat,
    MessagePartBody, Profile, Result, ThreadFormat,
};

impl GmailClient {
    /// `users.getProfile`: the address, totals and current history ID.
    pub async fn get_profile(&self) -> Result<Profile> {
        self.get_json(&["profile"], &[]).await
    }

    /// `labels.list`: every system and user label, without counts.
    pub async fn list_labels(&self) -> Result<Vec<ApiLabel>> {
        let response: ListLabelsResponse = self.get_json(&["labels"], &[]).await?;
        Ok(response.labels)
    }

    /// `labels.get`: one label, with message and thread counts.
    pub async fn get_label(&self, id: &LabelId) -> Result<ApiLabel> {
        self.get_json(&["labels", id.as_str()], &[]).await
    }

    /// `threads.list`: one page of thread IDs matching `request`, newest first.
    pub async fn list_threads(&self, request: &ListRequest) -> Result<ListThreadsResponse> {
        self.get_json(&["threads"], &request.to_query()).await
    }

    /// `threads.get`: a thread and its messages, oldest first.
    ///
    /// `metadata_headers` limits the headers returned with
    /// [`ThreadFormat::Metadata`] (all headers if empty), such as
    /// [`METADATA_HEADERS`](super::METADATA_HEADERS). Other formats ignore it.
    pub async fn get_thread(
        &self,
        id: &ThreadId,
        format: ThreadFormat,
        metadata_headers: &[&str],
    ) -> Result<ApiThread> {
        let query = format_query(format.as_str(), metadata_headers);
        self.get_json(&["threads", id.as_str()], &query).await
    }

    /// `messages.list`: one page of message IDs matching `request`, newest first.
    pub async fn list_messages(&self, request: &ListRequest) -> Result<ListMessagesResponse> {
        self.get_json(&["messages"], &request.to_query()).await
    }

    /// `messages.get`: one message in the given format. With
    /// [`MessageFormat::Raw`], decode the message with
    /// [`ApiMessage::decode_raw`].
    pub async fn get_message(
        &self,
        id: &MessageId,
        format: MessageFormat,
        metadata_headers: &[&str],
    ) -> Result<ApiMessage> {
        let query = format_query(format.as_str(), metadata_headers);
        self.get_json(&["messages", id.as_str()], &query).await
    }

    /// `messages.attachments.get`: an attachment's content. Decode it with
    /// [`MessagePartBody::decode_data`].
    ///
    /// Gmail attachment IDs change between fetches of a message, so use one
    /// from a recent `messages.get`.
    pub async fn get_attachment(
        &self,
        message_id: &MessageId,
        attachment_id: &AttachmentId,
    ) -> Result<MessagePartBody> {
        let segments = [
            "messages",
            message_id.as_str(),
            "attachments",
            attachment_id.as_str(),
        ];
        self.get_json(&segments, &[]).await
    }

    /// `history.list`: one page of mailbox changes after
    /// `request.start_history_id`, oldest first.
    ///
    /// Returns [`Error::HistoryExpired`] if Gmail no longer has history that
    /// far back (it answers `404`); the caller must then do a full sync.
    pub async fn list_history(&self, request: &HistoryRequest) -> Result<ListHistoryResponse> {
        self.get_json(&["history"], &request.to_query())
            .await
            .map_err(|err| match err {
                Error::NotFound { .. } => Error::HistoryExpired {
                    start: request.start_history_id,
                },
                other => other,
            })
    }
}
