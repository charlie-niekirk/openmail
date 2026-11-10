//! Messages built with `OutgoingMessage` parse back to what went in.

use std::path::Path;

use openmail_core::Address;
use openmail_mime::{
    InlineImage, OutgoingAttachment, OutgoingMessage, ParsedMessage, ThreadingHeaders,
    reply_subject,
};
use time::OffsetDateTime;

const GIF: &[u8] = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\x00\x00\x00\xff\xff\xff!\xf9\x04\x01\x00\x00\x00\x00,\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x01D\x00;";

fn full_message() -> OutgoingMessage {
    let mut message = OutgoingMessage::new(Address::named(
        "Zo\u{eb} \u{c5}str\u{f6}m",
        "zoe@example.com",
    ));
    message.to = vec![
        Address::named("Charles Babbage", "charles@example.org"),
        Address::new("team@example.org"),
    ];
    message.cc = vec![Address::named("Somerville, Mary", "mary@example.net")];
    message.bcc = vec![Address::new("archive@example.com")];
    message.reply_to = vec![Address::named("Analytical Engines", "engines@example.com")];
    message.subject = "Caf\u{e9} \u{2615} plans for the \u{1f680} launch, with a subject long enough to fold across lines".to_owned();
    message.text = "Hello \u{2705}\n\nA line that is long enough to need wrapping by quoted-printable encoding: \u{e9}\u{e8}\u{ea}\u{eb} \u{e9}\u{e8}\u{ea}\u{eb} \u{e9}\u{e8}\u{ea}\u{eb} \u{e9}\u{e8}\u{ea}\u{eb}.\n".to_owned();
    message.html =
        Some("<p>Hello \u{2705}</p><p><img src=\"cid:logo@example.com\"></p>\n".to_owned());
    message.inline_images = vec![InlineImage {
        content_id: "logo@example.com".to_owned(),
        mime_type: "image/gif".to_owned(),
        data: GIF.to_vec(),
    }];
    message.attachments = vec![
        OutgoingAttachment {
            filename: "r\u{e9}sum\u{e9}.pdf".to_owned(),
            mime_type: "application/pdf".to_owned(),
            data: (0..=255).collect(),
        },
        OutgoingAttachment {
            filename: "notes.txt".to_owned(),
            mime_type: "text/plain".to_owned(),
            data: b"plain\r\nnotes\r\n".to_vec(),
        },
    ];
    message.in_reply_to = Some("parent@example.org".to_owned());
    message.references = vec![
        "root@example.org".to_owned(),
        "parent@example.org".to_owned(),
    ];
    message.message_id = Some("built-1@example.com".to_owned());
    message.date = Some(OffsetDateTime::from_unix_timestamp(1_736_238_600).unwrap());
    message
}

#[test]
fn build_then_parse_preserves_everything() {
    let sent = full_message();
    let parsed = ParsedMessage::parse_base64url(&sent.to_base64url()).unwrap();
    let headers = &parsed.headers;

    assert_eq!(headers.from, std::slice::from_ref(&sent.from));
    assert_eq!(headers.to, sent.to);
    assert_eq!(headers.cc, sent.cc);
    assert_eq!(headers.bcc, sent.bcc);
    assert_eq!(headers.reply_to, sent.reply_to);
    assert_eq!(headers.subject.as_deref(), Some(sent.subject.as_str()));
    assert_eq!(headers.date, sent.date);
    assert_eq!(headers.message_id, sent.message_id);
    assert_eq!(headers.in_reply_to, ["parent@example.org"]);
    assert_eq!(headers.references, sent.references);

    let lf = |text: &str| text.replace("\r\n", "\n");
    assert_eq!(parsed.text.as_deref().map(lf), Some(sent.text.clone()));
    assert_eq!(parsed.html.as_deref().map(lf), sent.html.clone());

    let logo = &parsed.inline["logo@example.com"];
    assert_eq!(logo.mime_type, "image/gif");
    assert_eq!(logo.bytes(), GIF);
    assert!(logo.is_inline);
    assert_eq!(parsed.inline.len(), 1);

    assert_eq!(parsed.attachments.len(), sent.attachments.len());
    for (received, original) in parsed.attachments.iter().zip(&sent.attachments) {
        assert_eq!(
            received.filename.as_deref(),
            Some(original.filename.as_str())
        );
        assert_eq!(received.mime_type, original.mime_type);
        assert_eq!(received.bytes(), original.data.as_slice());
        assert!(!received.is_inline);
    }
}

#[test]
fn plain_text_only_round_trips() {
    let mut sent = OutgoingMessage::new(Address::new("ada@example.com"));
    sent.to = vec![Address::new("charles@example.org")];
    sent.subject = "Hi".to_owned();
    sent.text = "Just text.\n".to_owned();
    let parsed = ParsedMessage::parse(&sent.to_rfc5322()).unwrap();
    assert_eq!(parsed.text.as_deref(), Some("Just text.\r\n"));
    assert!(parsed.html.is_none());
    assert!(parsed.inline.is_empty() && parsed.attachments.is_empty());
    assert!(parsed.headers.message_id.unwrap().ends_with("@example.com"));
    assert!(parsed.headers.date.is_some());
}

#[test]
fn reply_to_a_parsed_fixture_threads_onto_it() {
    let raw =
        std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/plain-text.eml"))
            .unwrap();
    let parent = ParsedMessage::parse(&raw).unwrap();

    let mut reply = OutgoingMessage::new(Address::new("charles@example.org"));
    reply.to = parent.headers.reply_to.clone();
    reply.subject = reply_subject(parent.headers.subject.as_deref().unwrap_or_default());
    reply.text = "Thanks.\n".to_owned();
    ThreadingHeaders::from_parsed(&parent.headers).apply_to(&mut reply);

    let parsed = ParsedMessage::parse(&reply.to_rfc5322()).unwrap();
    assert_eq!(
        parsed.headers.subject.as_deref(),
        Some("Re: Notes on the engine")
    );
    assert_eq!(parsed.headers.in_reply_to, ["reply-2@mail.example.com"]);
    assert_eq!(
        parsed.headers.references,
        [
            "thread-0@mail.example.org",
            "thread-1@mail.example.org",
            "reply-2@mail.example.com",
        ]
    );
}
