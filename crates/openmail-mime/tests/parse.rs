//! Parsing tests over the synthetic `.eml` corpus in `tests/fixtures/`.

use std::path::Path;

use openmail_core::Address;
use openmail_mime::{Error, ParsedMessage, encode_base64url, html_to_text};

fn fixture_bytes(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn fixture(name: &str) -> ParsedMessage {
    ParsedMessage::parse(&fixture_bytes(name)).expect("fixture parses")
}

#[test]
fn plain_text() {
    let message = fixture("plain-text.eml");
    let headers = &message.headers;
    assert_eq!(
        headers.from,
        [Address::named("Ada Lovelace", "ada@example.com")]
    );
    assert_eq!(
        headers.cc,
        [
            Address::named("Somerville, Mary", "mary@example.net"),
            Address::new("notes@example.net"),
        ]
    );
    assert_eq!(
        headers.reply_to,
        [Address::named("Analytical Engines", "engines@example.com")]
    );
    assert_eq!(
        headers.message_id.as_deref(),
        Some("reply-2@mail.example.com")
    );
    assert_eq!(headers.in_reply_to, ["thread-1@mail.example.org"]);
    assert_eq!(
        headers.references,
        ["thread-0@mail.example.org", "thread-1@mail.example.org"]
    );
    let date = headers.date.expect("date");
    assert_eq!(date.unix_timestamp(), 1_736_238_600);
    assert_eq!(date.offset().whole_hours(), 1);
    assert!(message.html.is_none());
    assert!(message.inline.is_empty() && message.attachments.is_empty());
    insta::assert_debug_snapshot!(message);
}

#[test]
fn multipart_alternative() {
    let message = fixture("multipart-alternative.eml");
    let text = message.text.as_deref().expect("text body");
    assert!(text.contains("green \u{2705} and the caf\u{e9} is open"));
    let html = message.html.as_deref().expect("html body");
    assert!(html.contains("<b>green</b> \u{2705} and the caf\u{e9} is open"));
    assert!(message.attachments.is_empty());
    insta::assert_debug_snapshot!(message);
}

#[test]
fn newsletter_with_cid_images() {
    let message = fixture("newsletter-cid.eml");
    let html = message.html.as_deref().expect("html body");
    assert!(html.contains(r#"src="cid:logo@news.example.com""#));
    assert!(
        message
            .text
            .as_deref()
            .unwrap()
            .starts_with("Weekly digest")
    );

    // Both images are inline resources keyed by Content-ID, not attachments.
    assert_eq!(
        message.inline.keys().collect::<Vec<_>>(),
        ["chart.gif", "logo@news.example.com"]
    );
    assert!(message.attachments.is_empty());
    let logo = &message.inline["logo@news.example.com"];
    assert_eq!(logo.mime_type, "image/gif");
    assert_eq!(logo.filename.as_deref(), Some("logo.gif"));
    assert_eq!(logo.part_id, "1.1");
    assert!(logo.bytes().starts_with(b"GIF89a"));

    for reference in ["cid:chart.gif", "<chart.gif>", "chart.gif"] {
        let part = message.part_by_content_id(reference).expect(reference);
        assert_eq!(part.part_id, "1.2");
    }
    insta::assert_debug_snapshot!(message);
}

#[test]
fn attachments() {
    let message = fixture("attachments.eml");
    assert_eq!(
        message.text.as_deref(),
        Some("Your invoice and usage report are attached.\r\n")
    );
    assert!(message.inline.is_empty());

    let [pdf, csv, stamp] = &message.attachments[..] else {
        panic!("three attachments: {:?}", message.attachments);
    };
    assert_eq!(pdf.part_id, "1");
    assert_eq!(pdf.mime_type, "application/pdf");
    assert_eq!(pdf.filename.as_deref(), Some("invoice.pdf"));
    assert!(pdf.bytes().starts_with(b"%PDF-1.4"));
    assert!(!pdf.is_inline);

    // RFC 2231 encoded file name; quoted-printable content.
    assert_eq!(
        csv.filename.as_deref(),
        Some("r\u{e9}sum\u{e9} d'usage.csv")
    );
    assert_eq!(csv.mime_type, "text/csv");
    assert_eq!(csv.bytes(), b"name,amount\r\nwidgets,3\r\ngadgets,5\r\n");
    assert_eq!(csv.size(), csv.bytes().len());

    // `Content-Disposition: attachment` wins over a Content-ID, but the part
    // can still be found for a `cid:` reference.
    assert_eq!(
        stamp.content_id.as_deref(),
        Some("stamp@billing.example.com")
    );
    assert!(!stamp.is_inline);
    assert_eq!(
        message
            .part_by_content_id("cid:stamp@billing.example.com")
            .map(|part| part.part_id.as_str()),
        Some("3")
    );
    insta::assert_debug_snapshot!(message);
}

#[test]
fn latin1_and_windows1252_charsets() {
    let message = fixture("latin1-windows1252.eml");
    assert_eq!(
        message.headers.from,
        [Address::named("Ren\u{e9} Durand", "rene@example.fr")]
    );
    assert_eq!(
        message.headers.subject.as_deref(),
        Some("R\u{e9}union d\u{e9}plac\u{e9}e")
    );
    assert!(
        message
            .text
            .as_deref()
            .unwrap()
            .contains("d\u{e9}plac\u{e9}e \u{e0} 14 h. \u{c7}a te va ?")
    );
    assert!(
        message
            .html
            .as_deref()
            .unwrap()
            .contains("\u{20ac}12 \u{2013} \u{201c}quoted\u{201d}")
    );
    insta::assert_debug_snapshot!(message);
}

#[test]
fn iso_2022_jp_charset() {
    let message = fixture("iso-2022-jp.eml");
    assert_eq!(
        message.headers.from,
        [Address::named(
            "\u{5c71}\u{7530}\u{592a}\u{90ce}",
            "taro@example.jp"
        )]
    );
    assert_eq!(
        message.headers.subject.as_deref(),
        Some("\u{4f1a}\u{8b70}\u{306e}\u{304a}\u{77e5}\u{3089}\u{305b}")
    );
    assert!(
        message
            .text
            .as_deref()
            .unwrap()
            .starts_with("\u{3053}\u{3093}\u{306b}\u{3061}\u{306f}")
    );
    insta::assert_debug_snapshot!(message);
}

#[test]
fn encoded_word_headers() {
    let message = fixture("encoded-words.eml");
    let headers = &message.headers;
    // Adjacent encoded words join without the folding whitespace between them.
    assert_eq!(
        headers.subject.as_deref(),
        Some("Caf\u{e9} \u{2615} plans for the \u{1f680} launch")
    );
    assert_eq!(
        headers.from,
        [Address::named(
            "Zo\u{eb} \u{c5}str\u{f6}m",
            "zoe@example.com"
        )]
    );
    // Group members are flattened; empty groups contribute nobody.
    assert_eq!(
        headers.to,
        [
            Address::named("J\u{fc}rgen", "jurgen@example.org"),
            Address::named("Fran\u{e7}oise", "francoise@example.org"),
        ]
    );
    assert!(headers.cc.is_empty());
    insta::assert_debug_snapshot!(message);
}

#[test]
fn parses_gmail_base64url_raw() {
    let raw = fixture_bytes("multipart-alternative.eml");
    let from_raw = ParsedMessage::parse_base64url(&encode_base64url(&raw)).unwrap();
    assert_eq!(from_raw, ParsedMessage::parse(&raw).unwrap());
}

#[test]
fn rejects_empty_input_and_bad_base64url() {
    assert!(matches!(ParsedMessage::parse(b""), Err(Error::NotAMessage)));
    assert!(matches!(
        ParsedMessage::parse_base64url("*"),
        Err(Error::Base64(_))
    ));
}

#[test]
fn plain_text_falls_back_to_converted_html() {
    let raw = b"Content-Type: text/html; charset=utf-8\r\n\r\n<p>Hello <b>there</b></p>\r\n";
    let message = ParsedMessage::parse(raw).unwrap();
    assert!(message.text.is_none());
    assert_eq!(
        message.plain_text().as_deref().map(str::trim),
        Some("Hello there")
    );
}

#[test]
fn single_part_attachment_has_the_root_part_id() {
    let raw = b"Content-Type: application/pdf\r\n\
Content-Disposition: attachment; filename=\"a.pdf\"\r\n\
Content-Transfer-Encoding: base64\r\n\r\nJVBERg==\r\n";
    let message = ParsedMessage::parse(raw).unwrap();
    assert_eq!(message.attachments.len(), 1);
    assert_eq!(message.attachments[0].part_id, "");
    assert_eq!(message.attachments[0].bytes(), b"%PDF");
}

#[test]
fn html_to_text_decodes_references_and_drops_tags() {
    assert_eq!(
        html_to_text("It&#39;s <b>Ada</b> &amp; Bob").trim(),
        "It's Ada & Bob"
    );
    assert_eq!(html_to_text("Plain words").trim(), "Plain words");
}
