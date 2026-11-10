//! Thread, message, attachment and draft metadata.
//!
//! These hold what Gmail's `metadata` format returns, plus attachment metadata.
//! Message bodies are parsed separately (`openmail-mime`) and loaded lazily.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::{Address, AttachmentId, DraftId, HistoryId, LabelId, MessageId, SystemLabel, ThreadId};

/// The subset of a message's headers Openmail keeps with its metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageHeaders {
    /// `From`. Usually one address; RFC 5322 allows several.
    pub from: Vec<Address>,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub bcc: Vec<Address>,
    pub reply_to: Vec<Address>,
    pub subject: Option<String>,
    /// The RFC 5322 `Message-ID` header, such as `<abc@mail.example.com>`.
    pub message_id_header: Option<String>,
    /// The raw `In-Reply-To` header.
    pub in_reply_to: Option<String>,
    /// The raw `References` header.
    pub references: Option<String>,
}

/// A message's metadata, without its body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: MessageId,
    pub thread_id: ThreadId,
    /// The mailbox history ID of the message's last change.
    pub history_id: HistoryId,
    pub label_ids: Vec<LabelId>,
    /// A short plain-text excerpt of the body, as Gmail provides it.
    pub snippet: String,
    /// When Gmail received the message (or, for sent mail, when it was sent).
    #[serde(with = "time::serde::timestamp::milliseconds")]
    pub internal_date: OffsetDateTime,
    /// Gmail's estimate of the raw message size, in bytes.
    pub size_estimate: u64,
    pub headers: MessageHeaders,
    pub has_attachments: bool,
}

impl Message {
    /// Whether the message carries the given label.
    pub fn has_label(&self, label: &LabelId) -> bool {
        self.label_ids.contains(label)
    }

    /// Whether the message carries the given system label.
    pub fn has_system_label(&self, label: SystemLabel) -> bool {
        contains_system_label(&self.label_ids, label)
    }

    /// Whether the message is unread.
    pub fn is_unread(&self) -> bool {
        self.has_system_label(SystemLabel::Unread)
    }

    /// Whether the message is starred.
    pub fn is_starred(&self) -> bool {
        self.has_system_label(SystemLabel::Starred)
    }

    /// Whether Gmail marked the message important.
    pub fn is_important(&self) -> bool {
        self.has_system_label(SystemLabel::Important)
    }
}

/// A thread's metadata, summarised from its messages for the thread list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thread {
    pub id: ThreadId,
    /// The mailbox history ID of the thread's last change.
    pub history_id: HistoryId,
    /// The subject of the thread's first message.
    pub subject: Option<String>,
    /// An excerpt of the latest message, as Gmail provides it.
    pub snippet: String,
    /// The internal date of the latest message.
    #[serde(with = "time::serde::timestamp::milliseconds")]
    pub last_message_date: OffsetDateTime,
    pub message_count: u32,
    /// Every label on any message in the thread.
    pub label_ids: Vec<LabelId>,
    /// The senders of the thread's messages, oldest first, without duplicates.
    pub senders: Vec<Address>,
    /// Whether any message in the thread has attachments.
    pub has_attachments: bool,
}

impl Thread {
    /// Whether any message in the thread carries the given label.
    pub fn has_label(&self, label: &LabelId) -> bool {
        self.label_ids.contains(label)
    }

    /// Whether any message in the thread carries the given system label.
    pub fn has_system_label(&self, label: SystemLabel) -> bool {
        contains_system_label(&self.label_ids, label)
    }

    /// Whether the thread has unread messages.
    pub fn is_unread(&self) -> bool {
        self.has_system_label(SystemLabel::Unread)
    }

    /// Whether the thread has starred messages.
    pub fn is_starred(&self) -> bool {
        self.has_system_label(SystemLabel::Starred)
    }

    /// Whether Gmail marked any message in the thread important.
    pub fn is_important(&self) -> bool {
        self.has_system_label(SystemLabel::Important)
    }
}

/// An attachment's metadata. The content is fetched on demand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub message_id: MessageId,
    /// The MIME part ID within the message, such as `"1.2"`. Stable across fetches.
    pub part_id: String,
    /// The ID to fetch the content with. `None` when Gmail sent the content inline.
    pub attachment_id: Option<AttachmentId>,
    pub filename: String,
    pub mime_type: String,
    /// The decoded size, in bytes.
    pub size: u64,
    /// The `Content-ID`, without angle brackets, referenced by `cid:` URLs.
    pub content_id: Option<String>,
    /// Whether the part is `Content-Disposition: inline`, such as an embedded image.
    pub is_inline: bool,
}

/// A Gmail draft: an ID plus the unsent message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Draft {
    pub id: DraftId,
    pub message: Message,
}

fn contains_system_label(label_ids: &[LabelId], label: SystemLabel) -> bool {
    label_ids.iter().any(|id| id.is(label))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(labels: &[&str]) -> Message {
        Message {
            id: MessageId::new("m1"),
            thread_id: ThreadId::new("t1"),
            history_id: HistoryId::new(42),
            label_ids: labels.iter().copied().map(LabelId::from).collect(),
            snippet: "Hello".to_owned(),
            internal_date: OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
            size_estimate: 2048,
            headers: MessageHeaders {
                from: vec![Address::named("Ada", "ada@example.com")],
                subject: Some("Hi".to_owned()),
                ..MessageHeaders::default()
            },
            has_attachments: false,
        }
    }

    #[test]
    fn message_label_flags() {
        let m = message(&["INBOX", "UNREAD", "Label_1"]);
        assert!(m.is_unread());
        assert!(!m.is_starred());
        assert!(!m.is_important());
        assert!(m.has_system_label(SystemLabel::Inbox));
        assert!(m.has_label(&LabelId::new("Label_1")));

        let m = message(&["STARRED", "IMPORTANT"]);
        assert!(!m.is_unread());
        assert!(m.is_starred());
        assert!(m.is_important());
    }

    #[test]
    fn message_serde_round_trips_with_millisecond_dates() {
        let mut m = message(&["INBOX"]);
        m.internal_date =
            OffsetDateTime::from_unix_timestamp_nanos(1_700_000_000_123_000_000).unwrap();
        let json = serde_json::to_value(&m).unwrap();
        assert_eq!(json["internal_date"], 1_700_000_000_123_i64);
        assert_eq!(json["history_id"], "42");
        let back: Message = serde_json::from_value(json).unwrap();
        assert_eq!(back, m);
    }
}
