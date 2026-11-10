//! Conversions from Gmail API models into `openmail-core` domain types.

use openmail_core::{
    Address, Attachment, Label, LabelColor, LabelType, Message, MessageHeaders, SystemLabel, Thread,
};
use time::OffsetDateTime;

use super::{ApiLabel, ApiLabelColor, ApiMessage, ApiThread, Error, MessagePart, Result};

impl From<ApiLabelColor> for LabelColor {
    fn from(color: ApiLabelColor) -> Self {
        Self {
            text_color: color.text_color,
            background_color: color.background_color,
        }
    }
}

impl From<ApiLabel> for Label {
    /// Missing visibilities take Gmail's defaults. A missing `type` (which
    /// `labels.list` always sends) is inferred from the ID.
    fn from(label: ApiLabel) -> Self {
        let label_type = label.label_type.unwrap_or_else(|| {
            if SystemLabel::from_id(label.id.as_str()).is_some() {
                LabelType::System
            } else {
                LabelType::User
            }
        });
        Self {
            id: label.id,
            name: label.name,
            label_type,
            color: label.color.map(Into::into),
            label_list_visibility: label.label_list_visibility.unwrap_or_default(),
            message_list_visibility: label.message_list_visibility.unwrap_or_default(),
        }
    }
}

impl ApiMessage {
    /// The headers Openmail keeps, from `payload` (empty without one).
    ///
    /// Address headers that do not parse are kept as one address whose `email`
    /// is the raw header value, so a malformed header never drops a message.
    pub fn headers(&self) -> MessageHeaders {
        let Some(payload) = &self.payload else {
            return MessageHeaders::default();
        };
        let text = |name| payload.header(name).map(str::to_owned);
        let addresses = |name| {
            payload
                .header(name)
                .map(parse_addresses)
                .unwrap_or_default()
        };
        MessageHeaders {
            from: addresses("From"),
            to: addresses("To"),
            cc: addresses("Cc"),
            bcc: addresses("Bcc"),
            reply_to: addresses("Reply-To"),
            subject: text("Subject"),
            message_id_header: text("Message-ID"),
            in_reply_to: text("In-Reply-To"),
            references: text("References"),
        }
    }

    /// Attachment metadata from the part tree (`format=full`), including inline
    /// parts such as embedded images. Empty for other formats.
    pub fn attachments(&self) -> Vec<Attachment> {
        let Some(payload) = &self.payload else {
            return Vec::new();
        };
        payload
            .walk()
            .filter_map(|part| attachment(&self.id, part))
            .collect()
    }

    /// Whether the message has attachments other than inline parts.
    ///
    /// With `format=full` this looks at the parts. `metadata` has no parts, so a
    /// `multipart/mixed` top-level type is taken to mean attachments, as Gmail
    /// gives no flag for it.
    pub fn has_attachments(&self) -> bool {
        let Some(payload) = &self.payload else {
            return false;
        };
        self.attachments().iter().any(|a| !a.is_inline)
            || (payload.parts.is_empty()
                && payload.mime_type.eq_ignore_ascii_case("multipart/mixed"))
    }
}

impl TryFrom<&ApiMessage> for Message {
    type Error = Error;

    /// Needs at least the `minimal` format: `history_id` and `internal_date`
    /// must be set. Headers come from `metadata` or `full`.
    fn try_from(message: &ApiMessage) -> Result<Self> {
        let missing = |field| {
            Error::InvalidResponse(format!("message {} has no {field}", message.id.as_str()))
        };
        let history_id = message.history_id.ok_or_else(|| missing("historyId"))?;
        let internal_date = message
            .internal_date
            .ok_or_else(|| missing("internalDate"))?;
        Ok(Self {
            id: message.id.clone(),
            thread_id: message.thread_id.clone(),
            history_id,
            label_ids: message.label_ids.clone(),
            snippet: message.snippet.clone().unwrap_or_default(),
            internal_date: date_from_millis(internal_date)?,
            size_estimate: message.size_estimate.unwrap_or_default(),
            headers: message.headers(),
            has_attachments: message.has_attachments(),
        })
    }
}

impl TryFrom<ApiMessage> for Message {
    type Error = Error;

    fn try_from(message: ApiMessage) -> Result<Self> {
        Self::try_from(&message)
    }
}

impl ApiThread {
    /// Converts a `threads.get` result (any format but with messages) into a
    /// thread summary and its messages, oldest first.
    pub fn into_domain(self) -> Result<(Thread, Vec<Message>)> {
        let messages = self
            .messages
            .iter()
            .map(Message::try_from)
            .collect::<Result<Vec<_>>>()?;
        let (Some(first), Some(last)) = (messages.first(), messages.last()) else {
            return Err(Error::InvalidResponse(format!(
                "thread {} has no messages",
                self.id.as_str()
            )));
        };

        let history_id = self
            .history_id
            .or_else(|| messages.iter().map(|m| m.history_id).max())
            .expect("messages is not empty");
        let last_message_date = messages
            .iter()
            .map(|m| m.internal_date)
            .max()
            .expect("messages is not empty");

        let mut label_ids = Vec::new();
        let mut senders: Vec<Address> = Vec::new();
        for message in &messages {
            for label in &message.label_ids {
                if !label_ids.contains(label) {
                    label_ids.push(label.clone());
                }
            }
            for sender in &message.headers.from {
                if !senders
                    .iter()
                    .any(|s| s.email.eq_ignore_ascii_case(&sender.email))
                {
                    senders.push(sender.clone());
                }
            }
        }

        let thread = Thread {
            id: self.id,
            history_id,
            subject: first.headers.subject.clone(),
            snippet: if last.snippet.is_empty() {
                self.snippet.unwrap_or_default()
            } else {
                last.snippet.clone()
            },
            last_message_date,
            message_count: u32::try_from(messages.len()).unwrap_or(u32::MAX),
            label_ids,
            senders,
            has_attachments: messages.iter().any(|m| m.has_attachments),
        };
        Ok((thread, messages))
    }
}

impl TryFrom<ApiThread> for Thread {
    type Error = Error;

    /// See [`ApiThread::into_domain`].
    fn try_from(thread: ApiThread) -> Result<Self> {
        thread.into_domain().map(|(thread, _)| thread)
    }
}

/// An attachment for `part`, if it is one: it has a filename, is marked
/// `Content-Disposition: attachment`, or is a non-text part with a `Content-ID`.
fn attachment(message_id: &openmail_core::MessageId, part: &MessagePart) -> Option<Attachment> {
    let mime_type = part.mime_type.to_ascii_lowercase();
    if mime_type.starts_with("multipart/") {
        return None;
    }
    let disposition = part
        .header("Content-Disposition")
        .map(|d| {
            d.split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase()
        })
        .unwrap_or_default();
    let content_id = part.header("Content-ID").map(|id| {
        id.trim()
            .trim_start_matches('<')
            .trim_end_matches('>')
            .to_owned()
    });

    let is_attachment = !part.filename.is_empty()
        || disposition == "attachment"
        || (content_id.is_some() && !mime_type.starts_with("text/"));
    if !is_attachment {
        return None;
    }

    let body = part.body.as_ref();
    Some(Attachment {
        message_id: message_id.clone(),
        part_id: part.part_id.clone(),
        attachment_id: body.and_then(|b| b.attachment_id.clone()),
        filename: part.filename.clone(),
        mime_type: part.mime_type.clone(),
        size: body.map_or(0, |b| b.size),
        is_inline: disposition == "inline" || (disposition.is_empty() && content_id.is_some()),
        content_id,
    })
}

/// Parses an address-list header, falling back to the raw value.
fn parse_addresses(value: &str) -> Vec<Address> {
    Address::parse_list(value).unwrap_or_else(|_| {
        let raw = value.trim();
        if raw.is_empty() {
            Vec::new()
        } else {
            vec![Address::new(raw)]
        }
    })
}

fn date_from_millis(ms: i64) -> Result<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .map_err(|e| Error::InvalidResponse(format!("internalDate {ms} out of range: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Header;

    fn part(mime: &str, filename: &str, headers: &[(&str, &str)]) -> MessagePart {
        MessagePart {
            part_id: "1".to_owned(),
            mime_type: mime.to_owned(),
            filename: filename.to_owned(),
            headers: headers
                .iter()
                .map(|(name, value)| Header {
                    name: (*name).to_owned(),
                    value: (*value).to_owned(),
                })
                .collect(),
            ..MessagePart::default()
        }
    }

    #[test]
    fn body_parts_are_not_attachments() {
        let id = openmail_core::MessageId::new("m");
        assert!(attachment(&id, &part("text/plain", "", &[])).is_none());
        assert!(attachment(&id, &part("text/html", "", &[("Content-ID", "<x>")])).is_none());
        assert!(attachment(&id, &part("multipart/mixed", "", &[])).is_none());
    }

    #[test]
    fn inline_images_without_a_filename_are_attachments() {
        let id = openmail_core::MessageId::new("m");
        let a = attachment(&id, &part("image/png", "", &[("Content-ID", " <img1@x> ")])).unwrap();
        assert!(a.is_inline);
        assert_eq!(a.content_id.as_deref(), Some("img1@x"));
    }

    #[test]
    fn disposition_decides_attachment_and_inline() {
        let id = openmail_core::MessageId::new("m");
        let a = attachment(
            &id,
            &part(
                "text/calendar",
                "",
                &[("Content-Disposition", "ATTACHMENT; filename=\"x.ics\"")],
            ),
        )
        .unwrap();
        assert!(!a.is_inline);
        let a = attachment(
            &id,
            &part("image/gif", "a.gif", &[("Content-Disposition", "inline")]),
        )
        .unwrap();
        assert!(a.is_inline);
    }

    #[test]
    fn unparseable_address_headers_are_kept_raw() {
        assert_eq!(
            parse_addresses("Ada <ada@example.com>, bob@"),
            vec![Address::new("Ada <ada@example.com>, bob@")]
        );
        assert!(parse_addresses("  ").is_empty());
        assert!(parse_addresses("undisclosed-recipients:;").is_empty());
    }

    #[test]
    fn dates_convert_from_milliseconds() {
        let date = date_from_millis(1_699_266_600_123).unwrap();
        assert_eq!(date.unix_timestamp(), 1_699_266_600);
        assert_eq!(date.millisecond(), 123);
        assert!(date_from_millis(i64::MAX).is_err());
    }
}
