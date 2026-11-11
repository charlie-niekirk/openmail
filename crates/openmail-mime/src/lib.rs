//! RFC 5322 / MIME parsing of incoming mail and building of outgoing mail.
//!
//! [`ParsedMessage::parse`] turns a raw message into headers, decoded text and
//! HTML bodies, inline `cid:` parts and attachments. [`OutgoingMessage`] builds
//! mail to send; [`reply_subject`], [`forward_subject`] and [`ThreadingHeaders`]
//! prepare replies and forwards.
//!
//! Gmail exchanges whole messages as base64url text: the `raw` format of
//! `messages.get`, and the `raw` field of `messages.send` and `drafts`.
//! [`decode_base64url`] and [`encode_base64url`] convert between that text and
//! RFC 5322 bytes.

mod base64url;
mod build;
mod error;
mod parse;
mod reply;

pub use base64url::{decode_base64url, encode_base64url};
pub use build::{InlineImage, OutgoingAttachment, OutgoingMessage, generate_message_id};
pub use error::{Error, Result};
pub use parse::{ParsedHeaders, ParsedMessage, ParsedPart, html_to_text};
pub use reply::{ThreadingHeaders, forward_subject, parse_message_ids, reply_subject};
