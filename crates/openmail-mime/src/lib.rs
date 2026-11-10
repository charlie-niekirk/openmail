//! RFC 5322 / MIME parsing of incoming mail and building of outgoing mail.
//!
//! [`ParsedMessage::parse`] turns a raw message into headers, decoded text and
//! HTML bodies, inline `cid:` parts and attachments.
//!
//! Gmail exchanges whole messages as base64url text: the `raw` format of
//! `messages.get`, and the `raw` field of `messages.send` and `drafts`.
//! [`decode_base64url`] and [`encode_base64url`] convert between that text and
//! RFC 5322 bytes.

mod base64url;
mod error;
mod parse;

pub use base64url::{decode_base64url, encode_base64url};
pub use error::{Error, Result};
pub use parse::{ParsedHeaders, ParsedMessage, ParsedPart};
