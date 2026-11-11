//! The sanitizer over a small fixture corpus: a newsletter, hostile HTML,
//! and a MIME message with `cid:` images.

use openmail_mime::ParsedMessage;
use openmail_store::InlinePart;

use super::*;

const NEWSLETTER: &str = include_str!("fixtures/newsletter.html");
const HOSTILE: &str = include_str!("fixtures/hostile.html");
const CID_MESSAGE: &[u8] =
    include_bytes!("../../../openmail-mime/tests/fixtures/newsletter-cid.eml");

fn clean(html: &str, remote_images: bool) -> CleanHtml {
    clean_html(
        html,
        &CleanOptions {
            message_id: "m1",
            remote_images,
            inline: &[],
        },
    )
}

/// Every `src`, `background` and `url(…)` in `html`, as written.
fn image_sources(html: &str) -> Vec<&str> {
    let mut sources = Vec::new();
    for marker in ["src=\"", "background=\"", "url("] {
        for (start, _) in html.match_indices(marker) {
            let rest = &html[start + marker.len()..];
            let end = rest.find(['"', ')']).unwrap_or(rest.len());
            sources.push(&rest[..end]);
        }
    }
    sources
}

#[test]
fn scope_ids_are_safe_in_selectors() {
    assert_eq!(scope_id("18c2f0a9b3e4d5f6"), "b-18c2f0a9b3e4d5f6");
    assert_eq!(scope_id("a b\"<c>"), "b-a_b__c_");
}

#[test]
fn newsletters_keep_their_layout_and_scoped_styles() {
    let cleaned = clean(NEWSLETTER, false);
    let html = &cleaned.html;
    assert!(html.contains("<table class=\"container\""), "{html}");
    assert!(html.contains("cellpadding=\"0\""));
    assert!(html.contains("bgcolor=\"#f2f2f7\""));
    assert!(html.contains("<font face=\"Georgia\" color=\"#44445a\" size=\"2\">"));
    assert!(html.contains("<center>"));
    // The head's title doesn't turn into text; its stylesheet stays, scoped.
    assert!(!html.contains("The Weekly Digest</"));
    assert!(html.contains("#b-m1{margin:0;padding:0;background-color:#f2f2f7}"));
    assert!(html.contains("#b-m1 .container{width:600px;background:#ffffff}"));
    assert!(html.contains("@media screen and (max-width: 600px) {#b-m1 .container{"));
    assert!(html.contains("#b-m1 #b-m1-footer p{"));
    // IDs and the anchors to them are renamed together.
    assert!(html.contains("id=\"b-m1-stories\""));
    assert!(html.contains("href=\"#b-m1-stories\""));
    assert!(html.contains("<a name=\"b-m1-top\" rel=\"noopener noreferrer\">"));
    assert!(html.contains("href=\"#b-m1-top\""));
    // Links stay links; `target` goes (links open outside the webview).
    assert!(html.contains("href=\"https://news.example/r?u=rust-on-the-desktop\""));
    assert!(html.contains("href=\"mailto:editor@news.example\""));
    assert!(!html.contains("target="));
    insta::assert_snapshot!(html);
}

#[test]
fn remote_images_are_blocked_by_default() {
    let cleaned = clean(NEWSLETTER, false);
    assert!(cleaned.blocked_images);
    assert!(cleaned.image_urls.is_empty());
    assert_eq!(
        image_sources(&cleaned.html),
        Vec::<&str>::new(),
        "{}",
        cleaned.html
    );
    // The images keep their place and alt text.
    assert!(cleaned.html.contains(
        "<img style=\"display:block\" width=\"120\" height=\"40\" \
         alt=\"The Weekly Digest\" border=\"0\">"
    ));
    assert!(cleaned.html.contains("background-color:#2b59c3"));
}

#[test]
fn shown_images_are_kept_and_listed_for_the_csp() {
    let cleaned = clean(NEWSLETTER, true);
    assert!(!cleaned.blocked_images);
    assert_eq!(
        cleaned
            .image_urls
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        [
            "https://cdn.news.example/hero-bg.jpg",
            "https://cdn.news.example/hero.jpg",
            "https://cdn.news.example/logo.png",
            "https://t.news.example/open.gif?u=123",
        ]
    );
    assert!(
        cleaned
            .html
            .contains("<img src=\"https://cdn.news.example/logo.png\"")
    );
    assert!(
        cleaned
            .html
            .contains("background=\"https://cdn.news.example/hero-bg.jpg\"")
    );
    assert!(
        cleaned
            .html
            .contains("background-image:url(https://cdn.news.example/hero.jpg)")
    );
}

#[test]
fn hostile_html_is_neutralized() {
    for remote_images in [false, true] {
        let cleaned = clean(HOSTILE, remote_images);
        let html = &cleaned.html;
        for forbidden in [
            "<script",
            "<iframe",
            "<object",
            "<embed",
            "<video",
            "<audio",
            "<form",
            "<input",
            "<button",
            "<base",
            "<meta",
            "<link",
            "<svg",
            "<source",
            "srcset",
            "onload",
            "onclick",
            "onerror",
            "javascript:",
            "data:text",
            "file:",
            "ping=",
            "relative/",
            "position:fixed",
            "@import",
            "@font-face",
            "evil.example/import",
            "font.woff",
            "class=\"overlay om-header\"",
            "id=\"m-m1\"",
        ] {
            assert!(!html.contains(forbidden), "{forbidden} in {html}");
        }
        // Remote images are never loaded unless shown: background, inline
        // style, stylesheet, `<img>` (but not `<picture>`'s `data:` one).
        let sources = image_sources(html);
        if remote_images {
            assert!(sources.iter().all(|src| {
                src.starts_with("https://evil.example/") || src.starts_with("data:image/gif")
            }));
        } else {
            assert!(cleaned.blocked_images);
            assert_eq!(sources, ["data:image/gif;base64,R0lGODlhAQABAAAAACw="]);
        }
        // The shell's sections can't be closed from inside.
        assert!(!html.contains("</details>") && !html.contains("</main>"));
    }
    insta::assert_snapshot!(clean(HOSTILE, false).html);
}

#[test]
fn shell_classes_are_dropped_but_others_kept() {
    let cleaned = clean("<p class=\"lead OM-header om-x  note\">Hi</p>", false);
    assert_eq!(cleaned.html, "<p class=\"lead note\">Hi</p>");
    let cleaned = clean("<p class=\"om-only\">Hi</p>", false);
    assert_eq!(cleaned.html, "<p>Hi</p>");
}

#[test]
fn quoted_markup_is_not_a_stylesheet() {
    let cleaned = clean(
        "<p title=\"<style>a{}</style>\">x</p><style>p{color:red}</style>",
        false,
    );
    assert_eq!(
        cleaned.html,
        "<p title=\"&lt;style&gt;a{}&lt;/style&gt;\">x</p><style>#b-m1 p{color:red}</style>"
    );
}

#[test]
fn cid_images_become_data_urls() {
    let parsed = ParsedMessage::parse(CID_MESSAGE).unwrap();
    let inline: Vec<InlinePart> = parsed
        .inline
        .values()
        .map(|part| InlinePart {
            content_id: part.content_id.clone().unwrap(),
            mime_type: part.mime_type.clone(),
            data: part.bytes().to_vec(),
        })
        .collect();
    let html = parsed.html.as_deref().unwrap();
    assert!(html.contains("cid:"));
    let cleaned = clean_html(
        html,
        &CleanOptions {
            message_id: "m1",
            remote_images: false,
            inline: &inline,
        },
    );
    assert!(!cleaned.html.contains("cid:"), "{}", cleaned.html);
    assert_eq!(
        cleaned
            .html
            .matches("src=\"data:image/gif;base64,R0lGOD")
            .count(),
        2
    );
    assert!(!cleaned.blocked_images);
    insta::assert_snapshot!(cleaned.html);
}

#[test]
fn cid_urls_match_loosely_and_unknown_ones_are_dropped() {
    let inline = [InlinePart {
        content_id: "Logo@Example".to_owned(),
        mime_type: "image/jpg".to_owned(),
        data: vec![0xff, 0xd8, 0xff],
    }];
    let cleaned = clean_html(
        "<img src=\"cid:logo%40example\"><img src=\"CID:<Logo@Example>\">\
         <img src=\"cid:other@example\" alt=\"gone\">\
         <p style=\"background:url(cid:Logo@Example)\">x</p>",
        &CleanOptions {
            message_id: "m1",
            remote_images: false,
            inline: &inline,
        },
    );
    assert_eq!(
        cleaned.html,
        "<img src=\"data:image/jpeg;base64,/9j/\"><img src=\"data:image/jpeg;base64,/9j/\">\
         <img alt=\"gone\">\
         <p style=\"background:url(data:image/jpeg;base64,/9j/)\">x</p>"
    );
}

#[test]
fn inline_parts_of_other_types_are_not_shown() {
    let inline = [InlinePart {
        content_id: "page@x".to_owned(),
        mime_type: "text/html".to_owned(),
        data: b"<script>alert(1)</script>".to_vec(),
    }];
    let cleaned = clean_html(
        "<img src=\"cid:page@x\">",
        &CleanOptions {
            message_id: "m1",
            remote_images: true,
            inline: &inline,
        },
    );
    assert_eq!(cleaned.html, "<img>");
}
