//! The HTML document the reading pane's webview shows: a trusted shell holding
//! every message of the open conversation, one `<details>` section each.
//!
//! The native webview can't be clipped by GPUI, so a conversation is one
//! document in one webview rather than a webview per message. Each message is
//! a `<details>` element whose `<summary>` is its header (sender, recipients or
//! snippet, date); opening and closing one is the engine's own behaviour, so
//! it works with JavaScript off. The subject and labels stay in GPUI above it.
//!
//! Everything here is plain string building, tested without a window. Text
//! from the message (names, addresses, plain-text bodies) is escaped; HTML
//! bodies go through [`clean_html`](crate::sanitize::clean_html) first, and
//! sit in a wrapper ([`scope_id`]) their styles are scoped to.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use time::OffsetDateTime;

use crate::conversation::{BodyState, Conversation, MessageItem};
use crate::sanitize::scope_id;

/// Content Security Policy for every body document. Nothing loads from the
/// network: only inline styles and `data:` images and fonts are allowed, and no
/// script at all. [`document_csp`] adds the remote images the user chose to
/// show to `img-src`.
pub const BODY_CSP: &str = "default-src 'none'; img-src data:; style-src 'unsafe-inline'; \
                            font-src data:; form-action 'none'; base-uri 'none'";

/// [`BODY_CSP`] allowing `image_urls` as well: each one exactly (a source
/// with a full path matches only that path), so showing a message's images
/// allows nothing else. URLs that can't be written as a CSP source (no host,
/// credentials, not `http(s):`) stay blocked.
pub fn document_csp(image_urls: &BTreeSet<String>) -> String {
    let sources: BTreeSet<String> = image_urls
        .iter()
        .filter_map(|url| csp_source(url))
        .collect();
    if sources.is_empty() {
        return BODY_CSP.to_owned();
    }
    let sources: Vec<&str> = sources.iter().map(String::as_str).collect();
    BODY_CSP.replacen(
        "img-src data:",
        &format!("img-src data: {}", sources.join(" ")),
        1,
    )
}

/// `url` as a CSP source expression matching just that resource: scheme,
/// host, port and path, without the query (CSP ignores it) or fragment.
fn csp_source(url: &str) -> Option<String> {
    let url = url::Url::parse(url).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    let host = url.host_str()?;
    let port = url
        .port()
        .map(|port| format!(":{port}"))
        .unwrap_or_default();
    // CSP compares paths percent-decoded, so these can be encoded; written
    // as they are they would end the source, directive or policy.
    let path = url
        .path()
        .replace(';', "%3B")
        .replace(',', "%2C")
        .replace('\'', "%27");
    Some(format!("{}://{host}{port}{path}", url.scheme()))
}

/// What a message whose body is still loading shows in its section.
pub const LOADING_BODY: &str = "Loading message…";

/// The shell's own styles. Its classes all start with `om-`, which message
/// HTML may not use, and message CSS is scoped to the message's wrapper, so
/// a message can't restyle the shell or take on its styles. `.om-body`
/// contains its message's painting (`contain: paint`): whatever the message
/// positions or moves stays inside its box.
///
/// Wide mail fits the pane where it can, as in Gmail: images shrink to the
/// message's width, and never pass the pane's (`100vw` less the shell's 82px
/// of padding and borders; WebKit doesn't let a percentage alone shrink a
/// sized image's min-content width), keeping their aspect ratio; long words
/// and URLs break. What can't shrink (a fixed-width table) widens the thread instead
/// of overflowing its box (`.om-thread` is at least its content's
/// min-content width), so the document itself scrolls sideways, from
/// anywhere in it, rather than a scroller at the bottom of the message.
/// Message CSS outranks these defaults (its selectors start with an ID).
const SHELL_CSS: &str = "\
:root{color-scheme:light}\
html,body{margin:0;padding:0;background:#ffffff;color:#1f1f23}\
body{font:14px/1.45 -apple-system,BlinkMacSystemFont,'Segoe UI',Helvetica,Arial,sans-serif;\
overflow-wrap:break-word}\
img{max-width:100%}\
.om-thread{max-width:860px;min-width:min-content;margin:0 auto;padding:4px 24px 24px;\
box-sizing:border-box}\
.om-message{border:1px solid #e4e4e9;border-radius:8px;margin:0 0 12px;overflow:hidden}\
.om-header{display:block;list-style:none;cursor:pointer;padding:8px 16px}\
.om-header::-webkit-details-marker{display:none}\
.om-header:focus-visible{outline:2px solid #2b59c3;outline-offset:-2px}\
.om-header:hover{background:#f5f5f8}\
.om-message[open]>.om-header{padding-top:12px}\
.om-header-row{display:flex;align-items:baseline;gap:12px}\
.om-sender{flex:none;max-width:220px;font-weight:600;overflow:hidden;\
text-overflow:ellipsis;white-space:nowrap}\
.om-line{flex:1;min-width:0;color:#6b6b75;overflow:hidden;text-overflow:ellipsis;\
white-space:nowrap}\
.om-date{flex:none;color:#6b6b75;font-size:12px}\
.om-message:not([open]) .om-recipients,.om-message[open] .om-snippet{display:none}\
.om-participants{padding:0 16px 12px;color:#6b6b75;font-size:13px}\
.om-participants table{border-collapse:collapse}\
.om-participants th{font-weight:normal;text-align:left;vertical-align:top;\
padding:0 8px 0 0;width:36px}\
.om-participants td{padding:0;color:#1f1f23}\
.om-body{padding:12px 16px 16px;border-top:1px solid #e4e4e9;contain:paint}\
.om-mail{overflow-wrap:anywhere}\
.om-mail img{max-width:min(100%,calc(100vw - 82px));height:auto}\
.om-text{white-space:pre-wrap}\
.om-notice{margin:0;color:#6b6b75}";

/// Wraps an HTML fragment in the trusted document shell the webview loads:
/// charset, CSP ([`document_csp`] for `image_urls`) and the shell's styles.
/// `body_html` must be trusted, or built from sanitized HTML and
/// [`escape_html`] text.
pub fn body_document(body_html: &str, image_urls: &BTreeSet<String>) -> String {
    // Quotes in the policy stay readable; only what would end the attribute
    // is escaped.
    let csp = document_csp(image_urls)
        .replace('&', "&amp;")
        .replace('"', "&quot;");
    format!(
        "<!DOCTYPE html>\n<html><head><meta charset=\"utf-8\">\
         <meta http-equiv=\"Content-Security-Policy\" content=\"{csp}\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <style>{SHELL_CSS}</style></head><body>{body_html}</body></html>"
    )
}

/// The document for `conversation`: every message, oldest first, open if it
/// is expanded in the view-model. `dates` gives a message's short date (for
/// its header) and full date (among its participants).
pub fn conversation_document(
    conversation: &Conversation,
    dates: impl Fn(OffsetDateTime) -> (String, String),
) -> String {
    // A line per section keeps the document (and its snapshots) readable.
    let mut html = String::from("<main class=\"om-thread\">\n");
    let mut image_urls = BTreeSet::new();
    for message in &conversation.messages {
        let (short, full) = dates(message.date);
        message_section(&mut html, message, &short, &full);
        if let BodyState::Loaded(body) = &message.body
            && let Some(cleaned) = &body.html
        {
            image_urls.extend(cleaned.image_urls.iter().cloned());
        }
    }
    html.push_str("</main>");
    body_document(&html, &image_urls)
}

/// Appends one message's `<details>` section to `html`.
fn message_section(html: &mut String, message: &MessageItem, short_date: &str, full_date: &str) {
    let open = if message.expanded { " open" } else { "" };
    let _ = write!(
        html,
        "<details class=\"om-message\" id=\"m-{id}\"{open}>\
         <summary class=\"om-header\"><div class=\"om-header-row\">\
         <span class=\"om-sender\">{sender}</span>\
         <span class=\"om-line\"><span class=\"om-recipients\">{recipients}</span>\
         <span class=\"om-snippet\">{snippet}</span></span>\
         <span class=\"om-date\">{short}</span>\
         </div></summary>",
        id = escape_html(message.id.as_str()),
        sender = escape_html(&message.sender),
        recipients = escape_html(&message.recipients),
        snippet = escape_html(&message.snippet),
        short = escape_html(short_date),
    );

    html.push_str("<div class=\"om-participants\"><table>");
    let rows = [
        ("from", message.from.as_str()),
        ("to", message.to.as_str()),
        ("cc", message.cc.as_str()),
        ("date", full_date),
    ];
    for (name, value) in rows.into_iter().filter(|(_, value)| !value.is_empty()) {
        let _ = write!(
            html,
            "<tr><th>{name}</th><td>{}</td></tr>",
            escape_html(value)
        );
    }
    html.push_str("</table></div><div class=\"om-body\">");

    match &message.body {
        BodyState::Loaded(body) => match &body.html {
            Some(cleaned) => {
                let _ = write!(
                    html,
                    "<div class=\"om-mail\" id=\"{}\">{}</div>",
                    scope_id(message.id.as_str()),
                    cleaned.html
                );
            }
            None => {
                let _ = write!(
                    html,
                    "<div class=\"om-text\">{}</div>",
                    escape_html(&body.text)
                );
            }
        },
        BodyState::NotLoaded | BodyState::Loading => {
            let _ = write!(html, "<p class=\"om-notice\">{LOADING_BODY}</p>");
        }
        BodyState::Unavailable(reason) => {
            let _ = write!(html, "<p class=\"om-notice\">{}</p>", escape_html(reason));
        }
    }
    html.push_str("</div></details>\n");
}

/// `text` escaped for HTML text and double-quoted attribute values.
pub fn escape_html(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            c => escaped.push(c),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use openmail_core::{MessageId, ThreadId};

    use super::*;
    use crate::conversation::LoadedBody;
    use crate::sanitize::{CleanHtml, CleanOptions, clean_html};

    fn clean(html: &str) -> CleanHtml {
        clean_html(
            html,
            &CleanOptions {
                message_id: "m4",
                remote_images: false,
                inline: &[],
            },
        )
    }

    /// A sample newsletter body shaped like real email: a table layout with
    /// inline styles, a `data:` image and links.
    const SAMPLE_NEWSLETTER: &str = include_str!("body_document/sample_newsletter.html");

    fn item(id: &str, expanded: bool, body: BodyState) -> MessageItem {
        MessageItem {
            id: MessageId::new(id),
            sender: "Ada <Lovelace>".to_owned(),
            sender_address: Some("ada@example.com".to_owned()),
            from: "Ada <ada@example.com>".to_owned(),
            to: "me@example.com".to_owned(),
            cc: String::new(),
            recipients: "to me".to_owned(),
            date: OffsetDateTime::UNIX_EPOCH,
            snippet: "Tea & cake?".to_owned(),
            unread: false,
            expanded,
            show_images: false,
            body,
        }
    }

    fn conversation(messages: Vec<MessageItem>) -> Conversation {
        Conversation {
            thread_id: ThreadId::new("t1"),
            subject: "Plans".to_owned(),
            labels: Vec::new(),
            messages,
        }
    }

    fn dates(_: OffsetDateTime) -> (String, String) {
        ("Nov 3".to_owned(), "Tue, Nov 3, 2026, 3:07 PM".to_owned())
    }

    #[test]
    fn document_carries_csp_and_body() {
        let doc = body_document("<p>Hello</p>", &BTreeSet::new());
        assert!(doc.starts_with("<!DOCTYPE html>"));
        assert!(doc.contains(&format!("content=\"{BODY_CSP}\"")));
        assert!(doc.contains("<body><p>Hello</p></body>"));
    }

    #[test]
    fn shown_images_are_allowed_one_by_one() {
        let urls: BTreeSet<String> = [
            "https://cdn.example/logo.png?v=2#x",
            "http://cdn.example:8080/a;b,c'd.gif",
            "https://user:pw@cdn.example/secret.png",
            "https://cdn.example/dir/",
            "ftp://cdn.example/x.png",
            "not a url",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        assert_eq!(
            document_csp(&urls),
            "default-src 'none'; img-src data: http://cdn.example:8080/a%3Bb%2Cc%27d.gif \
             https://cdn.example/dir/ https://cdn.example/logo.png; \
             style-src 'unsafe-inline'; font-src data:; form-action 'none'; base-uri 'none'"
        );
        assert_eq!(document_csp(&BTreeSet::new()), BODY_CSP);
        let doc = body_document("", &urls);
        assert!(doc.contains("img-src data: http://cdn.example:8080/a%3Bb%2Cc%27d.gif"));
    }

    #[test]
    fn wide_mail_fits_or_scrolls_the_document() {
        let doc = body_document("", &BTreeSet::new());
        // Images shrink to the message and long words break…
        assert!(doc.contains(".om-mail{overflow-wrap:anywhere}"));
        assert!(doc.contains(".om-mail img{max-width:min(100%,calc(100vw - 82px));height:auto}"));
        // …and what can't shrink widens the thread, so the document scrolls
        // sideways rather than a scroller inside the message.
        assert!(doc.contains("min-width:min-content"));
        assert!(!doc.contains("overflow-x"));
        assert!(doc.contains(
            ".om-body{padding:12px 16px 16px;border-top:1px solid #e4e4e9;contain:paint}"
        ));
    }

    #[test]
    fn csp_blocks_scripts_and_network() {
        assert!(BODY_CSP.starts_with("default-src 'none'"));
        assert!(!BODY_CSP.contains("script-src"));
        assert!(!BODY_CSP.contains("http"));
    }

    #[test]
    fn escapes_text_and_attributes() {
        assert_eq!(
            escape_html(r#"<a href="x">Tom & 'Jerry'</a>"#),
            "&lt;a href=&quot;x&quot;&gt;Tom &amp; &#39;Jerry&#39;&lt;/a&gt;"
        );
        assert_eq!(escape_html("plain"), "plain");
    }

    #[test]
    fn a_conversation_is_one_document_of_sections() {
        let loaded = |text: &str, html: Option<&str>| {
            BodyState::Loaded(LoadedBody {
                text: text.to_owned(),
                html: html.map(clean),
                source: None,
            })
        };
        let doc = conversation_document(
            &conversation(vec![
                item("m1", false, loaded("First\n  line", None)),
                item("m2", false, BodyState::Loading),
                item(
                    "m3",
                    true,
                    BodyState::Unavailable("You're offline.".to_owned()),
                ),
                item(
                    "m4",
                    true,
                    loaded("Hi", Some("<p style=\"color:red\">Hi</p>")),
                ),
            ]),
            dates,
        );
        insta::assert_snapshot!(doc);
    }

    #[test]
    fn only_expanded_messages_start_open() {
        let doc = conversation_document(
            &conversation(vec![
                item("m1", false, BodyState::NotLoaded),
                item("m2", true, BodyState::NotLoaded),
            ]),
            dates,
        );
        assert!(doc.contains("<details class=\"om-message\" id=\"m-m1\">"));
        assert!(doc.contains("<details class=\"om-message\" id=\"m-m2\" open>"));
        assert_eq!(doc.matches("<details").count(), 2);
        assert_eq!(doc.matches("</details>").count(), 2);
    }

    #[test]
    fn message_text_cannot_inject_markup() {
        let mut message = item(
            "m1\"><script>",
            true,
            BodyState::Loaded(LoadedBody {
                text: "</div></details><h1>Fake header</h1>".to_owned(),
                html: None,
                source: None,
            }),
        );
        message.sender = "<img src=x>".to_owned();
        let doc = conversation_document(&conversation(vec![message]), dates);
        assert!(!doc.contains("<script"));
        assert!(!doc.contains("<img"));
        assert!(!doc.contains("<h1>"));
        assert_eq!(doc.matches("</details>").count(), 1);
    }

    #[test]
    fn cleaning_drops_active_content() {
        let cleaned = clean(
            "<script>alert(1)</script><style>body{display:none}</style>\
             <form action=\"https://evil.example\"><input name=\"q\"></form>\
             <iframe src=\"https://evil.example\"></iframe>\
             <p onclick=\"alert(1)\" class=\"om-header\" id=\"m-m1\">Hi</p>\
             <meta http-equiv=\"refresh\" content=\"0;url=https://evil.example\">\
             <base href=\"https://evil.example/\">",
        );
        // The stylesheet only reaches the message's own wrapper.
        assert_eq!(
            cleaned.html,
            "<style>#b-m4{display:none}</style><p id=\"b-m4-m-m1\">Hi</p>"
        );
    }

    #[test]
    fn cleaning_balances_tags() {
        let cleaned = clean("<div><b>bold</div></details></main><p>after");
        // The parser closes what the fragment left open, and drops end tags
        // it never opened.
        assert_eq!(cleaned.html, "<div><b>bold</b></div><p><b>after</b></p>");
    }

    #[test]
    fn cleaning_keeps_email_layout() {
        let cleaned = clean(SAMPLE_NEWSLETTER).html;
        assert!(cleaned.contains("<table"));
        assert!(cleaned.contains("cellpadding=\"0\""));
        assert!(cleaned.contains("style=\"background:#f2f2f7\""));
        assert!(cleaned.contains("<img src=\"data:image/"));
        assert!(cleaned.contains("href=\"https://"));
        assert!(!cleaned.contains("<!--"), "comments are dropped");
    }
}
