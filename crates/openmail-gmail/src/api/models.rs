//! Serde models mirroring Gmail's JSON resources.
//!
//! Fields Gmail omits for some formats or endpoints are `Option` or default to
//! empty, so one model serves every format. `From`/`TryFrom` conversions into
//! `openmail-core` types live in the `convert` module.

use std::fmt;

use base64::Engine as _;
use base64::alphabet::URL_SAFE;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use openmail_core::{
    AttachmentId, HistoryId, LabelId, LabelListVisibility, LabelType, MessageId,
    MessageListVisibility, ThreadId,
};
use serde::{Deserialize, Deserializer, Serialize, de};

use super::{Error, Result};

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

/// A label resource (`labels.list`, `labels.get`).
///
/// `labels.list` omits the counts; `labels.get` includes them.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiLabel {
    pub id: LabelId,
    pub name: String,
    #[serde(rename = "type")]
    pub label_type: Option<LabelType>,
    pub message_list_visibility: Option<MessageListVisibility>,
    pub label_list_visibility: Option<LabelListVisibility>,
    pub color: Option<ApiLabelColor>,
    pub messages_total: Option<u64>,
    pub messages_unread: Option<u64>,
    pub threads_total: Option<u64>,
    pub threads_unread: Option<u64>,
}

/// A label's color, as Gmail's JSON spells it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiLabelColor {
    pub text_color: String,
    pub background_color: String,
}

/// The `labels.list` response.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct ListLabelsResponse {
    #[serde(default)]
    pub labels: Vec<ApiLabel>,
}

/// A thread resource.
///
/// In `threads.list` results only `id`, `snippet` and `history_id` are set;
/// `threads.get` adds `messages`, oldest first.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiThread {
    pub id: ThreadId,
    pub snippet: Option<String>,
    pub history_id: Option<HistoryId>,
    #[serde(default)]
    pub messages: Vec<ApiMessage>,
}

/// The `threads.list` response.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListThreadsResponse {
    #[serde(default)]
    pub threads: Vec<ApiThread>,
    pub next_page_token: Option<String>,
    pub result_size_estimate: Option<u64>,
}

/// A message resource.
///
/// What is set depends on where it came from: `messages.list` and `history.list`
/// give `id` and `thread_id` (history adds `label_ids`); `minimal` adds the
/// labels, snippet, history ID, date and size; `metadata` adds `payload` with
/// headers only; `full` adds the whole part tree; `raw` adds `raw` instead of
/// `payload`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiMessage {
    pub id: MessageId,
    pub thread_id: ThreadId,
    #[serde(default)]
    pub label_ids: Vec<LabelId>,
    pub snippet: Option<String>,
    pub history_id: Option<HistoryId>,
    /// Milliseconds since the Unix epoch. Gmail sends it as a string.
    #[serde(default, deserialize_with = "opt_i64_from_str_or_number")]
    pub internal_date: Option<i64>,
    pub size_estimate: Option<u64>,
    pub payload: Option<MessagePart>,
    /// The whole RFC 5322 message, base64url-encoded (`format=raw` only).
    pub raw: Option<String>,
}

impl ApiMessage {
    /// Decodes [`raw`](Self::raw) into the RFC 5322 message bytes.
    pub fn decode_raw(&self) -> Result<Option<Vec<u8>>> {
        self.raw.as_deref().map(decode_base64url).transpose()
    }
}

/// The `messages.list` response.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListMessagesResponse {
    #[serde(default)]
    pub messages: Vec<ApiMessage>,
    pub next_page_token: Option<String>,
    pub result_size_estimate: Option<u64>,
}

/// One MIME part of a message, with its children.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessagePart {
    /// The part ID, such as `"1.2"`. Empty for the top-level part.
    #[serde(default)]
    pub part_id: String,
    #[serde(default)]
    pub mime_type: String,
    /// The attachment filename; empty if the part is not an attachment.
    #[serde(default)]
    pub filename: String,
    #[serde(default)]
    pub headers: Vec<Header>,
    pub body: Option<MessagePartBody>,
    #[serde(default)]
    pub parts: Vec<MessagePart>,
}

impl MessagePart {
    /// The value of the first header called `name`, ignoring ASCII case.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case(name))
            .map(|h| h.value.as_str())
    }

    /// This part and all its descendants, depth first.
    pub fn walk(&self) -> impl Iterator<Item = &MessagePart> {
        let mut stack = vec![self];
        std::iter::from_fn(move || {
            let part = stack.pop()?;
            stack.extend(part.parts.iter().rev());
            Some(part)
        })
    }
}

/// A message header.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Header {
    pub name: String,
    pub value: String,
}

/// A part's body, or an attachment's content (`messages.attachments.get`).
///
/// Small bodies arrive inline in `data`; larger ones and attachments have an
/// `attachment_id` to fetch them with.
#[derive(Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessagePartBody {
    pub attachment_id: Option<AttachmentId>,
    /// The decoded size, in bytes.
    #[serde(default)]
    pub size: u64,
    /// The content, base64url-encoded.
    pub data: Option<String>,
}

impl MessagePartBody {
    /// Decodes [`data`](Self::data), if present.
    pub fn decode_data(&self) -> Result<Option<Vec<u8>>> {
        self.data.as_deref().map(decode_base64url).transpose()
    }
}

impl fmt::Debug for MessagePartBody {
    // Bodies can be megabytes; show the length rather than the data.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MessagePartBody")
            .field("attachment_id", &self.attachment_id)
            .field("size", &self.size)
            .field("data_len", &self.data.as_ref().map(String::len))
            .finish()
    }
}

/// How much of each message `threads.get` returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThreadFormat {
    /// IDs, labels, snippet, history ID, date and size.
    Minimal,
    /// `Minimal` plus the headers named in `metadata_headers` (all if empty).
    Metadata,
    /// The whole parsed message, with inline bodies.
    #[default]
    Full,
}

impl ThreadFormat {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Metadata => "metadata",
            Self::Full => "full",
        }
    }
}

/// How much of a message `messages.get` returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MessageFormat {
    /// IDs, labels, snippet, history ID, date and size.
    Minimal,
    /// `Minimal` plus the headers named in `metadata_headers` (all if empty).
    Metadata,
    /// The whole parsed message, with inline bodies.
    #[default]
    Full,
    /// The whole RFC 5322 message, base64url-encoded, in `raw`.
    Raw,
}

impl MessageFormat {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Metadata => "metadata",
            Self::Full => "full",
            Self::Raw => "raw",
        }
    }
}

/// The headers Openmail keeps with message metadata; a good `metadata_headers`
/// value for the `Metadata` formats.
pub const METADATA_HEADERS: &[&str] = &[
    "From",
    "To",
    "Cc",
    "Bcc",
    "Reply-To",
    "Subject",
    "Date",
    "Message-ID",
    "In-Reply-To",
    "References",
];

/// Search parameters shared by `threads.list` and `messages.list`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[must_use]
pub struct ListRequest {
    /// A Gmail search query, such as `"from:ada is:unread"`.
    pub q: Option<String>,
    /// Only return results carrying all of these labels.
    pub label_ids: Vec<LabelId>,
    /// The page to fetch, from a previous response's `next_page_token`.
    pub page_token: Option<String>,
    /// Results per page. Gmail defaults to 100 and allows up to 500.
    pub max_results: Option<u32>,
    /// Include results from Spam and Trash.
    pub include_spam_trash: bool,
}

impl ListRequest {
    /// A request for every thread or message, in Gmail's default page size.
    pub fn new() -> Self {
        Self::default()
    }

    /// Filters with a Gmail search query.
    pub fn query(mut self, q: impl Into<String>) -> Self {
        self.q = Some(q.into());
        self
    }

    /// Requires a label (in addition to any already required).
    pub fn label(mut self, id: impl Into<LabelId>) -> Self {
        self.label_ids.push(id.into());
        self
    }

    /// Sets the page size.
    pub fn max_results(mut self, n: u32) -> Self {
        self.max_results = Some(n);
        self
    }

    /// Starts from this page token.
    pub fn page_token(mut self, token: impl Into<String>) -> Self {
        self.page_token = Some(token.into());
        self
    }

    /// Includes Spam and Trash.
    pub fn include_spam_trash(mut self, include: bool) -> Self {
        self.include_spam_trash = include;
        self
    }

    pub(crate) fn to_query(&self) -> Vec<(&'static str, String)> {
        let mut query = Vec::new();
        if let Some(q) = &self.q {
            query.push(("q", q.clone()));
        }
        query.extend(
            self.label_ids
                .iter()
                .map(|id| ("labelIds", id.as_str().to_owned())),
        );
        if let Some(token) = &self.page_token {
            query.push(("pageToken", token.clone()));
        }
        if let Some(n) = self.max_results {
            query.push(("maxResults", n.to_string()));
        }
        if self.include_spam_trash {
            query.push(("includeSpamTrash", "true".to_owned()));
        }
        query
    }
}

/// Labels to add to and remove from messages or threads, for
/// `messages.modify`, `messages.batchModify` and `threads.modify`.
///
/// Gmail applies the additions and removals together. Archiving is removing
/// `INBOX`; marking read is removing `UNREAD`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize)]
#[serde(rename_all = "camelCase")]
#[must_use]
pub struct ModifyLabels {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub add_label_ids: Vec<LabelId>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub remove_label_ids: Vec<LabelId>,
}

impl ModifyLabels {
    /// No changes.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a label.
    pub fn add_label(mut self, id: impl Into<LabelId>) -> Self {
        self.add_label_ids.push(id.into());
        self
    }

    /// Removes a label.
    pub fn remove_label(mut self, id: impl Into<LabelId>) -> Self {
        self.remove_label_ids.push(id.into());
        self
    }

    /// Whether there is nothing to add or remove.
    pub fn is_empty(&self) -> bool {
        self.add_label_ids.is_empty() && self.remove_label_ids.is_empty()
    }
}

/// The `format` and `metadataHeaders` query parameters.
pub(crate) fn format_query(format: &str, metadata_headers: &[&str]) -> Vec<(&'static str, String)> {
    let mut query = vec![("format", format.to_owned())];
    if format == "metadata" {
        query.extend(
            metadata_headers
                .iter()
                .map(|h| ("metadataHeaders", (*h).to_owned())),
        );
    }
    query
}

/// One change record from `history.list`.
///
/// Each record lists the messages it touched in `messages`, and what changed in
/// the typed lists (only the types requested are filled in).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct History {
    pub id: HistoryId,
    #[serde(default)]
    pub messages: Vec<ApiMessage>,
    #[serde(default)]
    pub messages_added: Vec<HistoryMessage>,
    #[serde(default)]
    pub messages_deleted: Vec<HistoryMessage>,
    #[serde(default)]
    pub labels_added: Vec<HistoryLabelChange>,
    #[serde(default)]
    pub labels_removed: Vec<HistoryLabelChange>,
}

/// A message added to or deleted from the mailbox.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct HistoryMessage {
    /// The message's `id`, `thread_id` and (when added) `label_ids`.
    pub message: ApiMessage,
}

/// Labels added to or removed from a message.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryLabelChange {
    /// The message, with its `label_ids` after the change.
    pub message: ApiMessage,
    /// The labels that were added or removed.
    pub label_ids: Vec<LabelId>,
}

/// The `history.list` response.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListHistoryResponse {
    #[serde(default)]
    pub history: Vec<History>,
    pub next_page_token: Option<String>,
    /// The mailbox's current history ID: where the next incremental sync starts
    /// once every page has been applied.
    pub history_id: HistoryId,
}

/// A kind of change `history.list` can report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HistoryType {
    MessageAdded,
    MessageDeleted,
    LabelAdded,
    LabelRemoved,
}

impl HistoryType {
    /// Every history type.
    pub const ALL: [Self; 4] = [
        Self::MessageAdded,
        Self::MessageDeleted,
        Self::LabelAdded,
        Self::LabelRemoved,
    ];

    const fn as_str(self) -> &'static str {
        match self {
            Self::MessageAdded => "messageAdded",
            Self::MessageDeleted => "messageDeleted",
            Self::LabelAdded => "labelAdded",
            Self::LabelRemoved => "labelRemoved",
        }
    }
}

/// Parameters for `history.list`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct HistoryRequest {
    /// Return changes after this history ID, typically the last one stored.
    pub start_history_id: HistoryId,
    /// Only report these kinds of change. Empty means all.
    pub history_types: Vec<HistoryType>,
    /// Only report changes to messages carrying this label.
    pub label_id: Option<LabelId>,
    /// The page to fetch, from a previous response's `next_page_token`.
    pub page_token: Option<String>,
    /// Records per page. Gmail defaults to 100 and allows up to 500.
    pub max_results: Option<u32>,
}

impl HistoryRequest {
    /// Every change after `start_history_id`.
    pub fn new(start_history_id: HistoryId) -> Self {
        Self {
            start_history_id,
            history_types: Vec::new(),
            label_id: None,
            page_token: None,
            max_results: None,
        }
    }

    /// Only reports these kinds of change.
    pub fn history_types(mut self, types: impl IntoIterator<Item = HistoryType>) -> Self {
        self.history_types = types.into_iter().collect();
        self
    }

    /// Only reports changes to messages carrying this label.
    pub fn label(mut self, id: impl Into<LabelId>) -> Self {
        self.label_id = Some(id.into());
        self
    }

    /// Starts from this page token.
    pub fn page_token(mut self, token: impl Into<String>) -> Self {
        self.page_token = Some(token.into());
        self
    }

    /// Sets the page size.
    pub fn max_results(mut self, n: u32) -> Self {
        self.max_results = Some(n);
        self
    }

    pub(crate) fn to_query(&self) -> Vec<(&'static str, String)> {
        let mut query = vec![("startHistoryId", self.start_history_id.to_string())];
        query.extend(
            self.history_types
                .iter()
                .map(|t| ("historyTypes", t.as_str().to_owned())),
        );
        if let Some(id) = &self.label_id {
            query.push(("labelId", id.as_str().to_owned()));
        }
        if let Some(token) = &self.page_token {
            query.push(("pageToken", token.clone()));
        }
        if let Some(n) = self.max_results {
            query.push(("maxResults", n.to_string()));
        }
        query
    }
}

/// Base64url as Gmail sends it: URL-safe alphabet, padding optional.
const BASE64URL: GeneralPurpose = GeneralPurpose::new(
    &URL_SAFE,
    GeneralPurposeConfig::new()
        .with_encode_padding(false)
        .with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

fn decode_base64url(data: &str) -> Result<Vec<u8>> {
    BASE64URL
        .decode(data)
        .map_err(|e| Error::InvalidResponse(format!("invalid base64url data: {e}")))
}

/// Accepts an `i64` sent as a JSON string (Gmail's encoding of int64) or number.
fn opt_i64_from_str_or_number<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StrOrNum {
        Str(String),
        Num(i64),
    }
    match Option::<StrOrNum>::deserialize(d)? {
        None => Ok(None),
        Some(StrOrNum::Num(n)) => Ok(Some(n)),
        Some(StrOrNum::Str(s)) => s.parse().map(Some).map_err(de::Error::custom),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_decodes_with_or_without_padding() {
        assert_eq!(decode_base64url("aGk_").unwrap(), b"hi?");
        assert_eq!(decode_base64url("aGk").unwrap(), b"hi");
        assert_eq!(decode_base64url("aGk=").unwrap(), b"hi");
        assert!(decode_base64url("a+b/").is_err());
    }

    #[test]
    fn internal_date_accepts_strings_and_numbers() {
        let m: ApiMessage =
            serde_json::from_str(r#"{"id":"m","threadId":"t","internalDate":"1700000000123"}"#)
                .unwrap();
        assert_eq!(m.internal_date, Some(1_700_000_000_123));
        let m: ApiMessage =
            serde_json::from_str(r#"{"id":"m","threadId":"t","internalDate":5}"#).unwrap();
        assert_eq!(m.internal_date, Some(5));
        let m: ApiMessage = serde_json::from_str(r#"{"id":"m","threadId":"t"}"#).unwrap();
        assert_eq!(m.internal_date, None);
    }

    #[test]
    fn list_request_builds_repeated_label_params() {
        let query = ListRequest::new()
            .query("is:unread")
            .label("INBOX")
            .label("Label_7")
            .max_results(50)
            .page_token("abc")
            .include_spam_trash(true)
            .to_query();
        assert_eq!(
            query,
            vec![
                ("q", "is:unread".to_owned()),
                ("labelIds", "INBOX".to_owned()),
                ("labelIds", "Label_7".to_owned()),
                ("pageToken", "abc".to_owned()),
                ("maxResults", "50".to_owned()),
                ("includeSpamTrash", "true".to_owned()),
            ]
        );
        assert!(ListRequest::new().to_query().is_empty());
    }

    #[test]
    fn metadata_headers_are_only_sent_with_the_metadata_format() {
        assert_eq!(
            format_query("metadata", &["From", "Subject"]),
            vec![
                ("format", "metadata".to_owned()),
                ("metadataHeaders", "From".to_owned()),
                ("metadataHeaders", "Subject".to_owned()),
            ]
        );
        assert_eq!(
            format_query("full", &["From"]),
            vec![("format", "full".to_owned())]
        );
    }

    #[test]
    fn history_request_builds_its_query() {
        let query = HistoryRequest::new(HistoryId::new(42))
            .history_types([HistoryType::MessageAdded, HistoryType::LabelRemoved])
            .label("INBOX")
            .page_token("p")
            .max_results(500)
            .to_query();
        assert_eq!(
            query,
            vec![
                ("startHistoryId", "42".to_owned()),
                ("historyTypes", "messageAdded".to_owned()),
                ("historyTypes", "labelRemoved".to_owned()),
                ("labelId", "INBOX".to_owned()),
                ("pageToken", "p".to_owned()),
                ("maxResults", "500".to_owned()),
            ]
        );
    }

    #[test]
    fn walk_visits_parts_depth_first() {
        let leaf = |id: &str| MessagePart {
            part_id: id.to_owned(),
            ..MessagePart::default()
        };
        let root = MessagePart {
            parts: vec![
                MessagePart {
                    part_id: "0".to_owned(),
                    parts: vec![leaf("0.0"), leaf("0.1")],
                    ..MessagePart::default()
                },
                leaf("1"),
            ],
            ..MessagePart::default()
        };
        let ids: Vec<_> = root.walk().map(|p| p.part_id.as_str()).collect();
        assert_eq!(ids, ["", "0", "0.0", "0.1", "1"]);
    }
}
