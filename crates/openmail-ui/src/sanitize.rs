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
//!   webview) and in-message anchors (renamed with the IDs), and notes the
//!   links whose text names another site than they go to
//!   ([`CleanHtml::misleading_links`]);
//! - decides how the message is shown in dark mode ([`dark`]), rewriting
//!   its colors for it if it is to be adapted.
//!
//! The document's CSP is the backstop: it allows no network loads except
//! the remote images of messages whose images are shown
//! ([`CleanHtml::image_urls`]).

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, PoisonError};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use openmail_store::InlinePart;

pub mod color;
pub mod css;
pub mod dark;

use dark::{DarkStyle, Probe};

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
    /// Links whose text reads as a web address on another site than the
    /// one they go to: the link (as [`link_key`] writes it) and the site
    /// its text names.
    pub misleading_links: BTreeMap<String, String>,
    /// How it is shown in dark mode.
    pub dark: DarkStyle,
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
    let (mut cleaned, probe) = clean_pass(html, options, Probe::default());
    if probe.needs_rewrite() {
        // Again, with its colors as custom properties for dark mode.
        (cleaned, _) = clean_pass(html, options, Probe::rewriting(&probe));
    }
    cleaned.dark = probe.style();
    cleaned
}

/// One cleaning of `html`, with `probe` learning (or rewriting) its
/// colors.
fn clean_pass(html: &str, options: &CleanOptions<'_>, probe: Probe) -> (CleanHtml, Probe) {
    let scope = scope_id(options.message_id);
    let id_prefix = format!("{scope}-");
    let state = Arc::new(Mutex::new(State {
        images: Images {
            remote: options.remote_images,
            inline: data_urls(options.inline),
            blocked: false,
            urls: BTreeSet::new(),
        },
        probe,
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
    let filter_state = state.clone();
    let filter_prefix = id_prefix.clone();
    builder.attribute_filter(move |element, attribute, value| {
        let mut state = filter_state.lock().unwrap_or_else(PoisonError::into_inner);
        filter_attribute(element, attribute, value, &filter_prefix, &mut state)
    });
    let cleaned = builder.clean(html).to_string();

    let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
    let scope_selector = format!("#{scope}");
    let mut misleading_links = BTreeMap::new();
    let html = post_process(
        &cleaned,
        |stylesheet| {
            css::scope_stylesheet(
                stylesheet,
                css::Scope {
                    selector: &scope_selector,
                    id_prefix: &id_prefix,
                },
                &mut *state,
            )
        },
        |href, text| {
            if let Some(claimed) = misleading_link(href, text)
                && let Some(key) = link_key(href)
            {
                misleading_links.insert(key, claimed);
            }
        },
    );
    let cleaned = CleanHtml {
        html,
        blocked_images: state.images.blocked,
        image_urls: std::mem::take(&mut state.images.urls),
        misleading_links,
        dark: DarkStyle::Card,
    };
    (cleaned, std::mem::take(&mut state.probe))
}

/// What cleaning keeps track of: images, and colors for dark mode.
struct State {
    images: Images,
    probe: Probe,
}

impl css::Visitor for State {
    fn url(&mut self, url: &str) -> Option<String> {
        self.images.resolve(url)
    }

    fn color(&mut self, role: color::Role, color: color::Rgba) -> Option<String> {
        self.probe.color(role, color)
    }

    fn unknown_color(&mut self, _role: color::Role) {
        self.probe.unknown_color();
    }

    fn background_image(&mut self) {
        self.probe.background_image();
    }

    fn dark_scheme(&mut self) {
        self.probe.dark_scheme();
    }
}

/// How a link is compared with the URL the webview reports when it is
/// clicked: parsed and written back, so both are normalized the same way.
/// `None` if it isn't an absolute URL.
pub fn link_key(url: &str) -> Option<String> {
    url::Url::parse(url.trim()).ok().map(String::from)
}

/// The site a link's visible `text` names, if it names one other than the
/// `http(s):` link `href` goes to (or a subdomain of it).
fn misleading_link(href: &str, text: &str) -> Option<String> {
    let target = url::Url::parse(href.trim()).ok()?;
    if !matches!(target.scheme(), "http" | "https") {
        return None;
    }
    let host = target.host_str()?.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let claimed = claimed_host(text)?;
    let same = host == claimed || host.ends_with(&format!(".{claimed}"));
    (!same).then_some(claimed)
}

/// The host `text` reads as, if it is a web address on its own:
/// `https://example.com/x`, `www.example.com` or `example.com/x`, without a
/// leading `www.`. Hosts are compared in their ASCII (punycode) form, so a
/// look-alike Unicode name doesn't match the site it imitates.
fn claimed_host(text: &str) -> Option<String> {
    let text = text
        .trim()
        .trim_start_matches(['(', '<', '['])
        .trim_end_matches(['.', ',', ')', '>', ']']);
    if text.is_empty() || text.contains(char::is_whitespace) || text.contains('@') {
        return None;
    }
    let host = match text.split_once("://") {
        Some((scheme, _)) if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") => {
            return None;
        }
        Some(_) => url::Url::parse(text).ok()?.host_str()?.to_owned(),
        None => {
            let host = text.split(['/', '?', '#']).next()?;
            let name = host.split(':').next()?;
            let labels: Vec<&str> = name.split('.').collect();
            let tld = labels.last()?;
            let looks_like_a_domain = labels.len() >= 2
                && labels.iter().all(|label| {
                    !label.is_empty() && label.chars().all(|c| c.is_alphanumeric() || c == '-')
                })
                && tld.chars().count() >= 2
                && tld.chars().all(char::is_alphabetic);
            if !looks_like_a_domain {
                return None;
            }
            url::Url::parse(&format!("http://{name}"))
                .ok()?
                .host_str()?
                .to_owned()
        }
    };
    let host = host.to_ascii_lowercase();
    Some(host.strip_prefix("www.").unwrap_or(&host).to_owned())
}

/// What an allowed attribute becomes: `None` drops it.
fn filter_attribute<'a>(
    element: &str,
    attribute: &str,
    value: &'a str,
    id_prefix: &str,
    state: &mut State,
) -> Option<Cow<'a, str>> {
    match (element, attribute) {
        // A `<style>` element's attributes (`media`, `type`) aren't needed.
        ("style", _) => None,
        (_, "style") => {
            let css = css::filter_declarations(value, state);
            (!css.is_empty()).then_some(Cow::Owned(css))
        }
        ("img", "src") => state.images.resolve(value).map(Cow::Owned),
        (_, "background") => {
            state.probe.background_image();
            state.images.resolve(value).map(Cow::Owned)
        }
        (_, "bgcolor" | "color") => {
            state.probe.attribute(element, attribute, value);
            Some(Cow::Borrowed(value))
        }
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
/// stylesheet replaced by `rewrite`'s; `link` hears each link's `href` and
/// visible text.
///
/// The serializer's output is regular: text escapes `<`, attribute values
/// are double-quoted (with `"` escaped), a `<style>` element's text runs to
/// the first `</style>`, which the parser made sure can't appear inside it,
/// and links don't nest.
fn post_process(
    html: &str,
    mut rewrite: impl FnMut(&str) -> String,
    mut link: impl FnMut(&str, &str),
) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find('<') {
        let end = tag_end(&rest[start..]).map_or(rest.len(), |len| start + len);
        let tag = &rest[start..end];
        let name = tag_name(tag);
        if name == "a"
            && let Some(href) = attribute(tag, "href")
        {
            let after = &rest[end..];
            let close = after.find("</a>").unwrap_or(after.len());
            link(&href, &text_content(&after[..close]));
        }
        out.push_str(&rest[..end]);
        rest = &rest[end..];
        if name == "style" {
            let close = rest.find("</style>").unwrap_or(rest.len());
            out.push_str(&rewrite(&rest[..close]));
            rest = &rest[close..];
        }
    }
    out.push_str(rest);
    out
}

/// The value of attribute `name` in serialized `tag`, unescaped.
fn attribute(tag: &str, name: &str) -> Option<String> {
    let marker = format!(" {name}=\"");
    let start = tag.find(&marker)? + marker.len();
    let len = tag[start..].find('"')?;
    Some(unescape(&tag[start..start + len]))
}

/// The text of serialized `html`, tags dropped and unescaped.
fn text_content(html: &str) -> String {
    let mut text = String::new();
    let mut rest = html;
    while let Some(start) = rest.find('<') {
        text.push_str(&rest[..start]);
        let end = tag_end(&rest[start..]).map_or(rest.len(), |len| start + len);
        rest = &rest[end..];
    }
    text.push_str(rest);
    unescape(&text)
}

/// Undoes the serializer's escapes.
fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&nbsp;", "\u{a0}")
        .replace("&amp;", "&")
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
