//! Parsing raw RFC 5322 messages into [`ParsedMessage`].

use std::collections::BTreeMap;
use std::fmt;

use mail_parser::{AddressList, MessageParser, MessagePart, MessageRef, PartKind, PartRole};
use openmail_core::Address;
use time::{OffsetDateTime, UtcOffset};

use crate::{Error, Result, decode_base64url};

/// A message parsed from its raw RFC 5322 form, with everything decoded.
///
/// Bodies are transfer-decoded and converted to UTF-8 from their declared
/// charset. Parts other than the bodies are split into [`inline`] parts, keyed
/// by `Content-ID` for rewriting `cid:` URLs, and [`attachments`].
///
/// [`inline`]: ParsedMessage::inline
/// [`attachments`]: ParsedMessage::attachments
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedMessage {
    pub headers: ParsedHeaders,
    /// The `text/plain` body. Several text parts (such as text around an
    /// inline image in `multipart/mixed`) are joined in reading order. `None`
    /// when the message has no `text/plain` body; see [`Self::plain_text`].
    pub text: Option<String>,
    /// The `text/html` body, with `<meta>` charset declarations rewritten to
    /// UTF-8 so a browser does not decode it a second time. Not sanitized.
    pub html: Option<String>,
    /// Inline parts with a `Content-ID`, keyed by that ID without angle
    /// brackets: the targets of `cid:` URLs in [`Self::html`].
    pub inline: BTreeMap<String, ParsedPart>,
    /// Every other non-body part: files to offer for download, including
    /// nested `message/rfc822` messages.
    pub attachments: Vec<ParsedPart>,
}

/// The headers of a [`ParsedMessage`], with RFC 2047 encoded words decoded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedHeaders {
    pub from: Vec<Address>,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub bcc: Vec<Address>,
    pub reply_to: Vec<Address>,
    pub subject: Option<String>,
    pub date: Option<OffsetDateTime>,
    /// `Message-ID`, without angle brackets.
    pub message_id: Option<String>,
    /// The message IDs of `In-Reply-To`, without angle brackets.
    pub in_reply_to: Vec<String>,
    /// The message IDs of `References`, oldest first, without angle brackets.
    pub references: Vec<String>,
}

/// A non-body MIME part: an inline image or an attachment.
#[derive(Clone, PartialEq, Eq)]
pub struct ParsedPart {
    /// The part's position in the MIME tree, numbered the way Gmail numbers
    /// `partId`: `""` for a single-part message, `"0"`, `"1"`, … for the
    /// children of the top-level multipart, `"1.0"` for their children.
    pub part_id: String,
    /// The lowercased MIME type, such as `image/png`.
    pub mime_type: String,
    /// The decoded file name, from `Content-Disposition` or `Content-Type`.
    pub filename: Option<String>,
    /// The `Content-ID`, without angle brackets.
    pub content_id: Option<String>,
    /// Whether the part is meant to be shown inside the message
    /// (`Content-Disposition: inline`, or a `multipart/related` resource).
    pub is_inline: bool,
    data: Vec<u8>,
}

impl ParsedMessage {
    /// Parses a raw RFC 5322 message.
    ///
    /// Malformed structure is tolerated as far as possible; only input with no
    /// message at all is rejected.
    pub fn parse(raw: &[u8]) -> Result<Self> {
        let message = MessageParser::new().parse(raw).ok_or(Error::NotAMessage)?;
        Ok(Self::from_message(message.root()))
    }

    /// Parses Gmail's base64url `raw` field (the `raw` format of `messages.get`).
    pub fn parse_base64url(raw: &str) -> Result<Self> {
        Self::parse(&decode_base64url(raw)?)
    }

    /// The text body, or text converted from the HTML body when the message
    /// has no `text/plain` part.
    pub fn plain_text(&self) -> Option<String> {
        self.text
            .clone()
            .or_else(|| self.html.as_deref().map(mail_parser::html_to_text))
    }

    /// Looks up an inline part, then an attachment, by its `Content-ID`.
    ///
    /// Accepts the ID bare, in angle brackets, or as a `cid:` URL.
    pub fn part_by_content_id(&self, content_id: &str) -> Option<&ParsedPart> {
        let id = content_id.strip_prefix("cid:").unwrap_or(content_id);
        let id = strip_angle_brackets(id);
        self.inline.get(id).or_else(|| {
            self.attachments
                .iter()
                .find(|part| part.content_id.as_deref() == Some(id))
        })
    }

    fn from_message(message: MessageRef<'_>) -> Self {
        let text = join(
            message
                .text_body()
                .filter(|part| matches!(part.kind(), PartKind::Text))
                .filter_map(|part| part.text()),
        );
        let html = join(
            message
                .html_body()
                .filter(|part| matches!(part.kind(), PartKind::Html))
                .filter_map(|part| part.html_utf8()),
        );

        let mut parts = Vec::new();
        collect_parts(message.root_part(), String::new(), &mut parts);
        let mut inline = BTreeMap::new();
        let mut attachments = Vec::new();
        for part in parts {
            match &part.content_id {
                Some(id) if part.is_inline && !inline.contains_key(id) => {
                    inline.insert(id.clone(), part);
                }
                _ => attachments.push(part),
            }
        }

        Self {
            headers: ParsedHeaders::from_message(message),
            text,
            html,
            inline,
            attachments,
        }
    }
}

impl ParsedHeaders {
    fn from_message(message: MessageRef<'_>) -> Self {
        let headers = message.headers();
        let ids = |list: Option<mail_parser::TextList<'_>>| {
            list.map(|list| list.iter().map(str::to_owned).collect())
                .unwrap_or_default()
        };
        Self {
            from: addresses(headers.from()),
            to: addresses(headers.to()),
            cc: addresses(headers.cc()),
            bcc: addresses(headers.bcc()),
            reply_to: addresses(headers.reply_to()),
            subject: headers.subject().map(str::to_owned),
            date: headers.date().and_then(to_offset_date_time),
            message_id: headers.message_id().map(str::to_owned),
            in_reply_to: ids(headers.in_reply_to()),
            references: ids(headers.references()),
        }
    }
}

impl ParsedPart {
    /// The decoded content.
    pub fn bytes(&self) -> &[u8] {
        &self.data
    }

    /// Takes the decoded content.
    pub fn into_bytes(self) -> Vec<u8> {
        self.data
    }

    /// The decoded size, in bytes.
    pub fn size(&self) -> usize {
        self.data.len()
    }

    fn from_part(part: MessagePart<'_>, part_id: String) -> Self {
        let mime_type = part
            .content_type()
            .map(|content_type| match content_type.subtype() {
                Some(subtype) => format!("{}/{subtype}", content_type.ctype()),
                None => content_type.ctype().to_owned(),
            })
            .unwrap_or_else(|| default_mime_type(&part).to_owned())
            .to_ascii_lowercase();
        Self {
            part_id,
            mime_type,
            filename: part.attachment_name().map(str::to_owned),
            content_id: part
                .content_id()
                .map(strip_angle_brackets)
                .filter(|id| !id.is_empty())
                .map(str::to_owned),
            is_inline: part.is_inline(),
            data: part.decoded().into_owned(),
        }
    }
}

/// Shows the size instead of the content, which may be megabytes of binary.
impl fmt::Debug for ParsedPart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ParsedPart")
            .field("part_id", &self.part_id)
            .field("mime_type", &self.mime_type)
            .field("filename", &self.filename)
            .field("content_id", &self.content_id)
            .field("is_inline", &self.is_inline)
            .field("size", &self.size())
            .finish()
    }
}

/// Walks the MIME tree, numbering parts like Gmail, and collects every leaf
/// that is not a body. Nested messages are kept whole, as attachments.
fn collect_parts(part: MessagePart<'_>, part_id: String, out: &mut Vec<ParsedPart>) {
    if part.is_multipart() {
        for (index, child) in part.children().enumerate() {
            let child_id = if part_id.is_empty() {
                index.to_string()
            } else {
                format!("{part_id}.{index}")
            };
            collect_parts(child, child_id, out);
        }
        return;
    }
    let is_body = match part.role() {
        PartRole::TextBody | PartRole::HtmlBody | PartRole::CopiedBody => true,
        // Alternatives that lost to a better body, such as a second
        // text/plain version. Other dropped parts (text/calendar) are kept.
        PartRole::Dropped => {
            part.is_content_type("text", "plain") || part.is_content_type("text", "html")
        }
        _ => false,
    };
    if !is_body {
        out.push(ParsedPart::from_part(part, part_id));
    }
}

/// The RFC 2045 default when a part has no Content-Type.
fn default_mime_type(part: &MessagePart<'_>) -> &'static str {
    match part.kind() {
        PartKind::Text => "text/plain",
        PartKind::Html => "text/html",
        PartKind::Message(_) => "message/rfc822",
        _ => "application/octet-stream",
    }
}

fn addresses(list: Option<AddressList<'_>>) -> Vec<Address> {
    let Some(list) = list else {
        return Vec::new();
    };
    list.mailboxes()
        .filter_map(|mailbox| {
            let email = mailbox.address()?.trim();
            if email.is_empty() {
                return None;
            }
            let name = mailbox
                .name()
                .map(str::trim)
                .filter(|name| !name.is_empty());
            Some(match name {
                Some(name) => Address::named(name, email),
                None => Address::new(email),
            })
        })
        .collect()
}

fn to_offset_date_time(date: mail_parser::DateTime) -> Option<OffsetDateTime> {
    if !date.is_valid() {
        return None;
    }
    let sign = if date.tz_before_gmt { -1 } else { 1 };
    let offset = UtcOffset::from_hms(
        sign * i8::try_from(date.tz_hour).ok()?,
        sign * i8::try_from(date.tz_minute).ok()?,
        0,
    )
    .ok()?;
    OffsetDateTime::from_unix_timestamp(date.to_timestamp())
        .ok()
        .map(|utc| utc.to_offset(offset))
}

fn join<'a>(parts: impl Iterator<Item = std::borrow::Cow<'a, str>>) -> Option<String> {
    let mut joined: Option<String> = None;
    for part in parts {
        match &mut joined {
            Some(joined) => {
                if !joined.ends_with('\n') {
                    joined.push('\n');
                }
                joined.push_str(&part);
            }
            None => joined = Some(part.into_owned()),
        }
    }
    joined
}

pub(crate) fn strip_angle_brackets(id: &str) -> &str {
    let id = id.trim();
    id.strip_prefix('<')
        .and_then(|id| id.strip_suffix('>'))
        .unwrap_or(id)
        .trim()
}
