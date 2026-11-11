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
//! bodies go through [`clean_html`] first.

use std::fmt::Write as _;
use std::sync::LazyLock;

use time::OffsetDateTime;

use crate::conversation::{BodyState, Conversation, MessageItem};

/// Content Security Policy for every body document. Nothing loads from the
/// network: only inline styles and `data:` images and fonts are allowed, and no
/// script at all.
pub const BODY_CSP: &str = "default-src 'none'; img-src data:; style-src 'unsafe-inline'; \
                            font-src data:; form-action 'none'; base-uri 'none'";

/// What a message whose body is still loading shows in its section.
pub const LOADING_BODY: &str = "Loading message…";

/// The shell's own styles. Its classes all start with `om-`, and
/// [`clean_html`] strips `class` and `id` from message HTML, so a message
/// can't restyle the shell or take on its styles.
const SHELL_CSS: &str = "\
:root{color-scheme:light}\
html,body{margin:0;padding:0;background:#ffffff;color:#1f1f23}\
body{font:14px/1.45 -apple-system,BlinkMacSystemFont,'Segoe UI',Helvetica,Arial,sans-serif;\
overflow-wrap:break-word}\
img{max-width:100%}\
.om-thread{max-width:860px;margin:0 auto;padding:4px 24px 24px;box-sizing:border-box}\
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
.om-body{padding:12px 16px 16px;border-top:1px solid #e4e4e9;overflow-x:auto}\
.om-text{white-space:pre-wrap}\
.om-notice{margin:0;color:#6b6b75}";

/// Wraps an HTML fragment in the trusted document shell the webview loads:
/// charset, CSP and the shell's styles. `body_html` must be trusted, or
/// built from [`clean_html`] output and [`escape_html`] text.
pub fn body_document(body_html: &str) -> String {
    format!(
        "<!DOCTYPE html>\n<html><head><meta charset=\"utf-8\">\
         <meta http-equiv=\"Content-Security-Policy\" content=\"{BODY_CSP}\">\
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
    for message in &conversation.messages {
        let (short, full) = dates(message.date);
        message_section(&mut html, message, &short, &full);
    }
    html.push_str("</main>");
    body_document(&html)
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
            Some(cleaned) => html.push_str(cleaned),
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

/// The sanitizer for message HTML: `ammonia`'s defaults (no script, style
/// element, form, frame or event handler; no `class` or `id`) plus the inline
/// styles and presentational attributes email layouts rely on, and `data:`
/// URLs for inline images.
///
/// This is an interim allowlist. PR 4.2 replaces it with the full email one
/// (CSS property filtering, remote images behind a prompt, `cid:` images).
static CLEANER: LazyLock<ammonia::Builder<'static>> = LazyLock::new(|| {
    let mut builder = ammonia::Builder::default();
    builder
        .add_tags(["center", "font"])
        .add_generic_attributes([
            "style", "align", "valign", "bgcolor", "width", "height", "border", "dir",
        ])
        .add_tag_attributes("table", ["cellpadding", "cellspacing"])
        .add_tag_attributes("font", ["color", "face", "size"])
        .add_url_schemes(["data"]);
    builder
});

/// `html` (a message's HTML part) sanitized into a fragment the shell can
/// hold. Parsing and re-serializing it also balances its tags, so it can't
/// close the message's section and spill into the shell.
pub fn clean_html(html: &str) -> String {
    CLEANER.clean(html).to_string()
}

#[cfg(test)]
mod tests {
    use openmail_core::{MessageId, ThreadId};

    use super::*;
    use crate::conversation::LoadedBody;

    /// A sample newsletter body shaped like real email: a table layout with
    /// inline styles, a `data:` image and links.
    const SAMPLE_NEWSLETTER: &str = include_str!("body_document/sample_newsletter.html");

    fn item(id: &str, expanded: bool, body: BodyState) -> MessageItem {
        MessageItem {
            id: MessageId::new(id),
            sender: "Ada <Lovelace>".to_owned(),
            from: "Ada <ada@example.com>".to_owned(),
            to: "me@example.com".to_owned(),
            cc: String::new(),
            recipients: "to me".to_owned(),
            date: OffsetDateTime::UNIX_EPOCH,
            snippet: "Tea & cake?".to_owned(),
            unread: false,
            expanded,
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
        let doc = body_document("<p>Hello</p>");
        assert!(doc.starts_with("<!DOCTYPE html>"));
        assert!(doc.contains(&format!("content=\"{BODY_CSP}\"")));
        assert!(doc.contains("<body><p>Hello</p></body>"));
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
                html: html.map(clean_html),
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
        let cleaned = clean_html(
            "<script>alert(1)</script><style>body{display:none}</style>\
             <form action=\"https://evil.example\"><input name=\"q\"></form>\
             <iframe src=\"https://evil.example\"></iframe>\
             <p onclick=\"alert(1)\" class=\"om-header\" id=\"m-m1\">Hi</p>\
             <meta http-equiv=\"refresh\" content=\"0;url=https://evil.example\">\
             <base href=\"https://evil.example/\">",
        );
        assert_eq!(cleaned, "<p>Hi</p>");
    }

    #[test]
    fn cleaning_balances_tags() {
        let cleaned = clean_html("<div><b>bold</div></details></main><p>after");
        // The parser closes what the fragment left open, and drops end tags
        // it never opened.
        assert_eq!(cleaned, "<div><b>bold</b></div><p><b>after</b></p>");
    }

    #[test]
    fn cleaning_keeps_email_layout() {
        let cleaned = clean_html(SAMPLE_NEWSLETTER);
        assert!(cleaned.contains("<table"));
        assert!(cleaned.contains("cellpadding=\"0\""));
        assert!(cleaned.contains("style=\"background:#f2f2f7;\""));
        assert!(cleaned.contains("<img src=\"data:image/"));
        assert!(cleaned.contains("href=\"https://"));
        assert!(!cleaned.contains("<!--"), "comments are dropped");
    }
}
