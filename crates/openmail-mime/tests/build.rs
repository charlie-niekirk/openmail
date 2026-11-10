//! Building outgoing messages.

use openmail_core::Address;
use openmail_mime::{
    InlineImage, OutgoingAttachment, OutgoingMessage, decode_base64url, generate_message_id,
};
use time::OffsetDateTime;

const GIF: &[u8] = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\x00\x00\x00\xff\xff\xff!\xf9\x04\x01\x00\x00\x00\x00,\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x01D\x00;";

fn message() -> OutgoingMessage {
    let mut message = OutgoingMessage::new(Address::named("Ada Lovelace", "ada@example.com"));
    message.to = vec![Address::named("Charles Babbage", "charles@example.org")];
    message.subject = "Notes on the engine".to_owned();
    message.text = "Charles,\n\nSee the notes.\n\nAda\n".to_owned();
    message.message_id = Some("fixed-1@example.com".to_owned());
    // 2025-01-07 08:30:00 UTC.
    message.date = Some(OffsetDateTime::from_unix_timestamp(1_736_238_600).unwrap());
    message
}

/// Replaces the random MIME boundaries with stable names, for snapshots.
fn normalize_boundaries(raw: &[u8]) -> String {
    let mut text = String::from_utf8(raw.to_vec()).expect("7-bit output");
    let mut boundaries = Vec::new();
    let mut rest = text.as_str();
    while let Some(start) = rest.find("boundary=\"") {
        rest = &rest[start + "boundary=\"".len()..];
        let end = rest.find('"').expect("closing quote");
        boundaries.push(rest[..end].to_owned());
        rest = &rest[end..];
    }
    for (index, boundary) in boundaries.iter().enumerate() {
        text = text.replace(boundary, &format!("BOUNDARY-{index}"));
    }
    text.replace("\r\n", "\n")
}

#[test]
fn plain_text_message() {
    let mut message = message();
    message.cc = vec![Address::new("notes@example.net")];
    message.bcc = vec![Address::new("archive@example.com")];
    message.reply_to = vec![Address::named("Analytical Engines", "engines@example.com")];
    message.in_reply_to = Some("<parent@example.org>".to_owned());
    message.references = vec![
        "root@example.org".to_owned(),
        "parent@example.org".to_owned(),
    ];

    let raw = message.to_rfc5322();
    let text = String::from_utf8(raw).unwrap();
    assert!(text.contains("\r\n"));
    assert!(
        !text.replace("\r\n", "").contains('\n'),
        "bare LF in output"
    );
    insta::assert_snapshot!(text.replace("\r\n", "\n"));
}

#[test]
fn html_with_inline_images_and_attachments() {
    let mut message = message();
    message.subject = "Caf\u{e9} \u{2615} menu".to_owned();
    message.html = Some(r#"<p>Menu: <img src="cid:menu@example.com"></p>"#.to_owned());
    message.inline_images = vec![InlineImage {
        content_id: "menu@example.com".to_owned(),
        mime_type: "image/gif".to_owned(),
        data: GIF.to_vec(),
    }];
    message.attachments = vec![OutgoingAttachment {
        filename: "r\u{e9}sum\u{e9}.pdf".to_owned(),
        mime_type: "application/pdf".to_owned(),
        data: b"%PDF-1.4\n%%EOF\n".to_vec(),
    }];
    insta::assert_snapshot!(normalize_boundaries(&message.to_rfc5322()));
}

#[test]
fn inline_images_without_html_are_attached_next_to_the_text() {
    let mut message = message();
    message.inline_images = vec![InlineImage {
        content_id: "orphan@example.com".to_owned(),
        mime_type: "image/gif".to_owned(),
        data: GIF.to_vec(),
    }];
    let raw = normalize_boundaries(&message.to_rfc5322());
    assert!(raw.contains("Content-Type: multipart/mixed"));
    assert!(raw.contains("Content-ID: <orphan@example.com>"));
    assert!(!raw.contains("multipart/related"));
}

#[test]
fn base64url_encodes_the_rfc5322_bytes() {
    let message = message();
    let encoded = message.to_base64url();
    assert!(!encoded.contains(['+', '/', '\n']));
    assert_eq!(decode_base64url(&encoded).unwrap(), message.to_rfc5322());
}

#[test]
fn generates_message_ids_and_dates_when_unset() {
    let mut message = message();
    message.message_id = None;
    message.date = None;
    let text = String::from_utf8(message.to_rfc5322()).unwrap();
    let id_line = text
        .lines()
        .find(|line| line.starts_with("Message-ID: "))
        .unwrap();
    assert!(id_line.ends_with("@example.com>"), "{id_line}");
    assert!(text.lines().any(|line| line.starts_with("Date: ")));

    let from = Address::new("ada@example.com");
    assert_ne!(generate_message_id(&from), generate_message_id(&from));
    assert!(generate_message_id(&Address::new("no-domain")).ends_with("@openmail.invalid"));
}
