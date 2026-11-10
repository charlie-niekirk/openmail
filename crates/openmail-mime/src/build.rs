//! Building outgoing messages for Gmail's `messages.send` and `drafts`.

use std::fmt;

use mail_builder::MessageBuilder;
use mail_builder::headers::address::Address as BuilderAddress;
use mail_builder::headers::date::Date;
use mail_builder::mime::{MimePart, make_boundary};
use openmail_core::Address;
use time::OffsetDateTime;

use crate::encode_base64url;
use crate::parse::strip_angle_brackets;

/// A message to send or save as a draft.
///
/// The structure follows what Gmail itself sends: `text/plain` alone, or a
/// `multipart/alternative` of text and HTML, with inline images in a
/// `multipart/related` next to the HTML, and the whole wrapped in
/// `multipart/mixed` when there are attachments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingMessage {
    pub from: Address,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    /// Written as a `Bcc` header: Gmail delivers to these recipients and
    /// removes the header before sending.
    pub bcc: Vec<Address>,
    pub reply_to: Vec<Address>,
    pub subject: String,
    /// The `text/plain` body. Always present, as the fallback for the HTML.
    pub text: String,
    /// An optional `text/html` alternative to [`Self::text`].
    pub html: Option<String>,
    /// Images referenced from [`Self::html`] by `cid:` URLs.
    pub inline_images: Vec<InlineImage>,
    pub attachments: Vec<OutgoingAttachment>,
    /// The parent's message ID, without angle brackets, when replying or
    /// forwarding. See [`crate::ThreadingHeaders`].
    pub in_reply_to: Option<String>,
    /// The thread's message IDs, oldest first, without angle brackets.
    pub references: Vec<String>,
    /// The `Message-ID`, without angle brackets. Generated from the sender's
    /// domain when `None`; see [`generate_message_id`].
    pub message_id: Option<String>,
    /// The `Date`. The current time when `None`.
    pub date: Option<OffsetDateTime>,
}

/// An image embedded in the HTML body and referenced by `cid:<content_id>`.
#[derive(Clone, PartialEq, Eq)]
pub struct InlineImage {
    /// The `Content-ID`, without angle brackets or `cid:`.
    pub content_id: String,
    /// The MIME type, such as `image/png`.
    pub mime_type: String,
    pub data: Vec<u8>,
}

/// A file attached to an outgoing message.
#[derive(Clone, PartialEq, Eq)]
pub struct OutgoingAttachment {
    pub filename: String,
    /// The MIME type, such as `application/pdf`.
    pub mime_type: String,
    pub data: Vec<u8>,
}

impl OutgoingMessage {
    /// A message from `from` with no recipients, an empty subject and body.
    pub fn new(from: Address) -> Self {
        Self {
            from,
            to: Vec::new(),
            cc: Vec::new(),
            bcc: Vec::new(),
            reply_to: Vec::new(),
            subject: String::new(),
            text: String::new(),
            html: None,
            inline_images: Vec::new(),
            attachments: Vec::new(),
            in_reply_to: None,
            references: Vec::new(),
            message_id: None,
            date: None,
        }
    }

    /// Builds the RFC 5322 message, with CRLF line endings.
    pub fn to_rfc5322(&self) -> Vec<u8> {
        let message_id = self
            .message_id
            .as_deref()
            .map(strip_angle_brackets)
            .map_or_else(|| generate_message_id(&self.from), str::to_owned);
        let date = self
            .date
            .map_or_else(Date::now, |date| Date::new(date.unix_timestamp()));

        let mut builder = MessageBuilder::new()
            .from(builder_address(&self.from))
            .subject(self.subject.as_str())
            .date(date)
            .message_id(message_id);
        if !self.to.is_empty() {
            builder = builder.to(builder_addresses(&self.to));
        }
        if !self.cc.is_empty() {
            builder = builder.cc(builder_addresses(&self.cc));
        }
        if !self.bcc.is_empty() {
            builder = builder.bcc(builder_addresses(&self.bcc));
        }
        if !self.reply_to.is_empty() {
            builder = builder.reply_to(builder_addresses(&self.reply_to));
        }
        if let Some(parent) = &self.in_reply_to {
            builder = builder.in_reply_to(strip_angle_brackets(parent));
        }
        if !self.references.is_empty() {
            let ids: Vec<&str> = self
                .references
                .iter()
                .map(|id| strip_angle_brackets(id))
                .collect();
            builder = builder.references(ids);
        }

        let mut output = Vec::new();
        builder.body(self.body()).serialize(&mut output);
        output
    }

    /// Builds the message as base64url, for the `raw` field of Gmail's
    /// `messages.send` and `drafts.create`/`drafts.update`.
    pub fn to_base64url(&self) -> String {
        encode_base64url(&self.to_rfc5322())
    }

    fn body(&self) -> MimePart<'_> {
        let text = MimePart::new("text/plain", self.text.as_str());
        let inline_images = self.inline_images.iter().map(|image| {
            MimePart::new(image.mime_type.as_str(), image.data.as_slice())
                .inline()
                .cid(strip_angle_brackets(&image.content_id))
        });

        // Without HTML there is nothing to reference inline images, so they
        // travel next to the text like attachments.
        let (body, orphaned_images) = match &self.html {
            Some(html) => {
                let html = MimePart::new("text/html", html.as_str());
                let html = if self.inline_images.is_empty() {
                    html
                } else {
                    let mut related = vec![html];
                    related.extend(inline_images);
                    MimePart::new("multipart/related", related)
                };
                (
                    MimePart::new("multipart/alternative", vec![text, html]),
                    Vec::new(),
                )
            }
            None => (text, inline_images.collect()),
        };

        let attachments = self.attachments.iter().map(|attachment| {
            MimePart::new(attachment.mime_type.as_str(), attachment.data.as_slice())
                .attachment(attachment.filename.as_str())
        });
        if orphaned_images.is_empty() && self.attachments.is_empty() {
            return body;
        }
        let mut mixed = vec![body];
        mixed.extend(orphaned_images);
        mixed.extend(attachments);
        MimePart::new("multipart/mixed", mixed)
    }
}

/// Generates a unique `Message-ID` (without angle brackets) on the sender's
/// domain, so the local host name never appears in sent mail.
pub fn generate_message_id(from: &Address) -> String {
    let domain = from
        .email
        .rsplit_once('@')
        .map(|(_, domain)| domain.trim())
        .filter(|domain| !domain.is_empty())
        .unwrap_or("openmail.invalid");
    format!("{}@{domain}", make_boundary("."))
}

fn builder_address(address: &Address) -> BuilderAddress<'_> {
    BuilderAddress::new_address(address.name.as_deref(), address.email.as_str())
}

fn builder_addresses(addresses: &[Address]) -> BuilderAddress<'_> {
    BuilderAddress::new_list(addresses.iter().map(builder_address).collect())
}

/// Shows the size instead of the content.
impl fmt::Debug for InlineImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InlineImage")
            .field("content_id", &self.content_id)
            .field("mime_type", &self.mime_type)
            .field("size", &self.data.len())
            .finish()
    }
}

/// Shows the size instead of the content.
impl fmt::Debug for OutgoingAttachment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutgoingAttachment")
            .field("filename", &self.filename)
            .field("mime_type", &self.mime_type)
            .field("size", &self.data.len())
            .finish()
    }
}
