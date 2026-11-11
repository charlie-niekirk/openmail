//! Sanitizing a message's HTML for the conversation document.
//!
//! [`clean_html`] runs the HTML through an email allowlist (`ammonia`): the
//! tags, presentational attributes, inline styles and `<style>` elements
//! newsletters are built from, and nothing that runs code, submits, embeds or
//! navigates by itself. On top of that it
//!
//! - **scopes the message's CSS** to the message's wrapper (see [`css`]):
//!   selectors are prefixed with it, `id`s take its prefix, and classes the
//!   shell uses (`om-…`) are dropped, so neither side styles the other;
//! - **blocks remote images** (`<img src>`, `background`, CSS `url()`)
//!   unless they are shown for this message, and reports that it did, so the
//!   reading pane can offer to show them;
//! - **rewrites `cid:` images** to `data:` URLs from the message's inline
//!   parts;
//! - keeps links to `http(s):` and `mailto:` only (opened outside the
//!   webview) and in-message anchors (renamed with the IDs).
//!
//! The document's CSP is the backstop: it allows no network loads except
//! the remote images of messages whose images are shown
//! ([`CleanHtml::image_urls`]).

use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, PoisonError};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use openmail_store::InlinePart;

pub mod css;

/// The image types a `data:` URL may carry. An SVG loaded as an image runs
/// no script and loads nothing else.
const IMAGE_TYPES: [&str; 6] = [
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "image/bmp",
    "image/svg+xml",
];

/// Tags email layouts use on top of `ammonia`'s defaults.
const EXTRA_TAGS: [&str; 5] = ["big", "center", "font", "section", "style"];

/// Elements dropped with their content (on top of `<script>`): a `<title>`
/// in the email's head would otherwise show as text.
const DROPPED_WITH_CONTENT: [&str; 4] = ["title", "template", "noembed", "noframes"];

/// Attributes on every allowed element.
const GENERIC_ATTRIBUTES: [&str; 12] = [
    "style",
    "class",
    "id",
    "align",
    "valign",
    "bgcolor",
    "background",
    "width",
    "height",
    "border",
    "dir",
    "nowrap",
];

/// Element-specific attributes on top of `ammonia`'s defaults.
const TAG_ATTRIBUTES: [(&str, &[&str]); 10] = [
    ("a", &["name"]),
    ("table", &["cellpadding", "cellspacing", "frame", "rules"]),
    ("td", &["abbr", "scope"]),
    ("font", &["color", "face", "size"]),
    ("img", &["border", "hspace", "vspace"]),
    ("hr", &["noshade", "color"]),
    ("ol", &["type", "reversed"]),
    ("ul", &["type"]),
    ("li", &["type", "value"]),
    ("col", &["width"]),
];

/// URL schemes that survive `ammonia`'s check, for the filter below to sort
/// out: links keep `http(s):` and `mailto:`, images `data:` and `cid:` (and
/// `http(s):` when shown).
const URL_SCHEMES: [&str; 5] = ["http", "https", "mailto", "data", "cid"];

/// What to clean a message's HTML for.
#[derive(Debug, Clone, Copy)]
pub struct CleanOptions<'a> {
    /// The message, which names its scope.
    pub message_id: &'a str,
    /// Whether remote images load (the user chose to show them).
    pub remote_images: bool,
    /// The message's inline parts, for `cid:` URLs.
    pub inline: &'a [InlinePart],
}

/// A message's HTML, sanitized into a fragment for its wrapper
/// ([`scope_id`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanHtml {
    pub html: String,
    /// Whether remote images were left out (they weren't to be shown).
    pub blocked_images: bool,
    /// The remote images kept (they are to be shown), for the CSP.
    pub image_urls: BTreeSet<String>,
}

/// The `id` of the element wrapping a message's HTML, which its CSS is
/// scoped to. Its IDs are prefixed with this and a `-`.
pub fn scope_id(message_id: &str) -> String {
    let id: String = message_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("b-{id}")
}

/// Sanitizes `html`, a message's HTML part, into a fragment for its wrapper.
/// Re-serializing it also balances its tags, so it can't close its section
/// and spill into the shell.
pub fn clean_html(html: &str, options: &CleanOptions<'_>) -> CleanHtml {
    let scope = scope_id(options.message_id);
    let id_prefix = format!("{scope}-");
    let images = Arc::new(Mutex::new(Images {
        remote: options.remote_images,
        inline: data_urls(options.inline),
        blocked: false,
        urls: BTreeSet::new(),
    }));

    let mut builder = ammonia::Builder::default();
    builder
        .add_tags(EXTRA_TAGS)
        .rm_clean_content_tags(["style"])
        .add_clean_content_tags(DROPPED_WITH_CONTENT)
        .add_generic_attributes(GENERIC_ATTRIBUTES)
        .url_schemes(URL_SCHEMES.into_iter().collect());
    for (tag, attributes) in TAG_ATTRIBUTES {
        builder.add_tag_attributes(tag, attributes);
    }
    let filter_images = images.clone();
    let filter_prefix = id_prefix.clone();
    builder.attribute_filter(move |element, attribute, value| {
        let mut images = filter_images.lock().unwrap_or_else(PoisonError::into_inner);
        filter_attribute(element, attribute, value, &filter_prefix, &mut images)
    });
    let cleaned = builder.clean(html).to_string();

    let mut images = images.lock().unwrap_or_else(PoisonError::into_inner);
    let scope_selector = format!("#{scope}");
    let html = rewrite_styles(&cleaned, |stylesheet| {
        css::scope_stylesheet(
            stylesheet,
            css::Scope {
                selector: &scope_selector,
                id_prefix: &id_prefix,
            },
            &mut |url| images.resolve(url),
        )
    });
    CleanHtml {
        html,
        blocked_images: images.blocked,
        image_urls: std::mem::take(&mut images.urls),
    }
}

/// What an allowed attribute becomes: `None` drops it.
fn filter_attribute<'a>(
    element: &str,
    attribute: &str,
    value: &'a str,
    id_prefix: &str,
    images: &mut Images,
) -> Option<Cow<'a, str>> {
    match (element, attribute) {
        // A `<style>` element's attributes (`media`, `type`) aren't needed.
        ("style", _) => None,
        (_, "style") => {
            let css = css::filter_declarations(value, &mut |url| images.resolve(url));
            (!css.is_empty()).then_some(Cow::Owned(css))
        }
        ("img", "src") | (_, "background") => images.resolve(value).map(Cow::Owned),
        ("a", "href") => link(value, id_prefix),
        (_, "id") | ("a", "name") => Some(Cow::Owned(format!("{id_prefix}{value}"))),
        (_, "class") => {
            let classes: Vec<&str> = value
                .split_ascii_whitespace()
                .filter(|class| !class.to_ascii_lowercase().starts_with("om-"))
                .collect();
            (!classes.is_empty()).then(|| Cow::Owned(classes.join(" ")))
        }
        _ => Some(Cow::Borrowed(value)),
    }
}

/// A link's `href`: `http(s):` and `mailto:` links as they are, anchors in
/// the message renamed as its IDs are. Anything else (relative links, which
/// would resolve against the document's `about:blank`, other schemes) is
/// dropped.
fn link<'a>(href: &'a str, id_prefix: &str) -> Option<Cow<'a, str>> {
    let href = href.trim();
    if let Some(fragment) = href.strip_prefix('#') {
        return Some(if fragment.is_empty() {
            Cow::Borrowed(href)
        } else {
            Cow::Owned(format!("#{id_prefix}{fragment}"))
        });
    }
    let scheme = href
        .split_once(':')
        .map(|(scheme, _)| scheme.to_ascii_lowercase());
    matches!(scheme.as_deref(), Some("http" | "https" | "mailto")).then_some(Cow::Borrowed(href))
}

/// Where a message's images come from, and what was blocked.
struct Images {
    /// Whether remote images may load.
    remote: bool,
    /// `data:` URLs for the message's inline parts, by `Content-ID`.
    inline: HashMap<String, String>,
    blocked: bool,
    urls: BTreeSet<String>,
}

impl Images {
    /// What an image URL becomes: `None` to leave the image out.
    fn resolve(&mut self, url: &str) -> Option<String> {
        let url = url.trim();
        let (scheme, rest) = url.split_once(':')?;
        match scheme.to_ascii_lowercase().as_str() {
            "data" => is_image_data(rest).then(|| url.to_owned()),
            "cid" => {
                let id = percent_decode(rest);
                let id = id.trim().trim_start_matches('<').trim_end_matches('>');
                self.inline
                    .get(id)
                    .or_else(|| self.inline.get(&id.to_ascii_lowercase()))
                    .cloned()
            }
            "http" | "https" if self.remote => {
                self.urls.insert(url.to_owned());
                Some(url.to_owned())
            }
            "http" | "https" => {
                self.blocked = true;
                None
            }
            _ => None,
        }
    }
}

/// Whether a `data:` URL's body (after `data:`) is an allowed image type.
fn is_image_data(rest: &str) -> bool {
    let media_type = rest
        .split([';', ','])
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    IMAGE_TYPES.contains(&media_type.as_str())
}

/// `data:` URLs for `parts` of an allowed image type, by `Content-ID` (also
/// lowercased, as some mailers change its case in the HTML).
fn data_urls(parts: &[InlinePart]) -> HashMap<String, String> {
    let mut urls = HashMap::new();
    for part in parts {
        let mime_type = match part.mime_type.as_str() {
            "image/jpg" | "image/pjpeg" => "image/jpeg",
            mime_type => mime_type,
        };
        if !IMAGE_TYPES.contains(&mime_type) {
            continue;
        }
        let url = format!("data:{mime_type};base64,{}", BASE64.encode(&part.data));
        urls.entry(part.content_id.to_ascii_lowercase())
            .or_insert_with(|| url.clone());
        urls.insert(part.content_id.clone(), url);
    }
    urls
}

/// `text` with `%XX` escapes decoded (as in `cid:` URLs, RFC 2392).
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(byte)) => {
                decoded.push(byte);
                i += 3;
            }
            (byte, _) => {
                decoded.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// `html` (`ammonia`'s serialized output) with each `<style>` element's
/// stylesheet replaced by `rewrite`'s.
///
/// The serializer's output is regular: text escapes `<`, attribute values
/// are double-quoted, and a `<style>` element's text runs to the first
/// `</style>`, which the parser made sure can't appear inside it.
fn rewrite_styles(html: &str, mut rewrite: impl FnMut(&str) -> String) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find('<') {
        let end = tag_end(&rest[start..]).map_or(rest.len(), |len| start + len);
        let is_style = tag_name(&rest[start..end]) == "style";
        out.push_str(&rest[..end]);
        rest = &rest[end..];
        if is_style {
            let close = rest.find("</style>").unwrap_or(rest.len());
            out.push_str(&rewrite(&rest[..close]));
            rest = &rest[close..];
        }
    }
    out.push_str(rest);
    out
}

/// The length of the tag `html` starts with, up to and including its `>`
/// (skipping `>` inside quoted attribute values).
fn tag_end(html: &str) -> Option<usize> {
    let mut quoted = false;
    for (i, byte) in html.bytes().enumerate() {
        match byte {
            b'"' => quoted = !quoted,
            b'>' if !quoted => return Some(i + 1),
            _ => {}
        }
    }
    None
}

/// The name of `tag` (`<name …>`), as in `p` or `/p`.
fn tag_name(tag: &str) -> &str {
    let name = tag.get(1..).unwrap_or_default();
    let len = name
        .find(|c: char| c.is_ascii_whitespace() || c == '>')
        .unwrap_or(name.len());
    &name[..len]
}

#[cfg(test)]
mod tests;
