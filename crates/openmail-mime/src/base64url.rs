//! Base64url, the encoding Gmail uses for raw RFC 5322 messages.

use base64::Engine as _;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_PAD_INDIFFERENT};

use crate::Result;

/// Encodes bytes as padded base64url (RFC 4648 §5), the form Gmail's
/// `messages.send` and `drafts` accept in their `raw` field.
pub fn encode_base64url(bytes: &[u8]) -> String {
    URL_SAFE.encode(bytes)
}

/// Decodes base64url text, such as the `raw` field of a Gmail message.
///
/// Lenient about what Gmail and other encoders emit: padding is optional,
/// ASCII whitespace (line breaks) is ignored, and the standard alphabet's `+`
/// and `/` are accepted in place of `-` and `_`.
pub fn decode_base64url(text: &str) -> Result<Vec<u8>> {
    let normalized: Vec<u8> = text
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .map(|byte| match byte {
            b'+' => b'-',
            b'/' => b'_',
            other => other,
        })
        .collect();
    Ok(URL_SAFE_PAD_INDIFFERENT.decode(normalized)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_bytes() {
        let bytes: Vec<u8> = (0..=255).collect();
        let encoded = encode_base64url(&bytes);
        assert!(!encoded.contains(['+', '/']));
        assert_eq!(decode_base64url(&encoded).unwrap(), bytes);
    }

    #[test]
    fn uses_the_url_safe_alphabet_with_padding() {
        assert_eq!(encode_base64url(&[0xfb, 0xff]), "-_8=");
    }

    #[test]
    fn accepts_missing_padding_line_breaks_and_the_standard_alphabet() {
        assert_eq!(decode_base64url("-_8").unwrap(), [0xfb, 0xff]);
        assert_eq!(decode_base64url("+/8=").unwrap(), [0xfb, 0xff]);
        assert_eq!(decode_base64url("SGVs\r\nbG8=\n").unwrap(), b"Hello");
    }

    #[test]
    fn rejects_invalid_text() {
        assert!(matches!(
            decode_base64url("not*base64"),
            Err(crate::Error::Base64(_))
        ));
    }
}
