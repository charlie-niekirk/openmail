//! Finding a message's quoted history and signature, to collapse them
//! behind a "…" toggle as Gmail does.
//!
//! Replies carry the conversation so far, quoted at the end. [`find`]
//! looks in a message's cleaned HTML for the markup mail clients put
//! around it:
//!
//! - Gmail: `<div class="gmail_quote">` (the "On … wrote:" line and the
//!   quote), or a `blockquote.gmail_quote` after a `div.gmail_attr`;
//! - Apple Mail and Thunderbird: `<blockquote type="cite">`, with the
//!   attribution before it (`div.moz-cite-prefix`, or a short line ending
//!   in "wrote:");
//! - Outlook: everything from `#appendonsend` or `#divRplyFwdMsg` (and the
//!   rule before it) to the end of its parent;
//! - Yahoo: `div.yahoo_quoted`.
//!
//! Only quoted history at the end of a message counts, after something of
//! the message's own: an inline reply (text after the quote) and a forward
//! stay as they are. [`split_text`] does the same for plain text: an "On …
//! wrote:" line and the `>` lines after it, or Outlook's "-----Original
//! Message-----" and what follows.
//!
//! The signature just before (Gmail's `div.gmail_signature`, or a plain
//! `-- ` line) is found too. The document collapses it only when an earlier
//! message in the conversation ended with the same one.

use std::ops::Range;

use super::{attribute, tag_end, tag_name, text_content};

/// Where a message's quoted history and signature are in its cleaned HTML.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Trimmed {
    /// The quoted history, to collapse.
    pub quote: Option<Range<usize>>,
    /// The signature before it, or ending the message.
    pub signature: Option<Signature>,
}

/// A message's signature in its cleaned HTML.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub range: Range<usize>,
    /// Its text, whitespace collapsed ([`normalize`]).
    pub text: String,
    /// Whether it sits beside the quote, in the same parent, so one toggle
    /// can hold both.
    pub joins_quote: bool,
}

/// HTML elements without end tags, as the serializer writes them.
const VOID: [&str; 14] = [
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

/// An element of serialized HTML.
#[derive(Debug)]
struct Element<'a> {
    name: &'a str,
    /// Its start tag.
    tag: &'a str,
    /// From its start tag to the end of its end tag.
    range: Range<usize>,
    /// Where its end tag starts (its end, for void elements).
    close: usize,
    parent: Option<usize>,
}

impl Element<'_> {
    fn has_class(&self, class: &str) -> bool {
        attribute(self.tag, "class")
            .is_some_and(|classes| classes.split_ascii_whitespace().any(|c| c == class))
    }
}

/// The elements of `html` (`ammonia`'s output: balanced, attributes
/// double-quoted, no comments, `<` escaped in text and stylesheets).
fn elements(html: &str) -> Vec<Element<'_>> {
    let mut elements: Vec<Element<'_>> = Vec::new();
    let mut open: Vec<usize> = Vec::new();
    let mut at = 0;
    while let Some(offset) = html[at..].find('<') {
        let start = at + offset;
        let len = tag_end(&html[start..]).unwrap_or(html.len() - start);
        let tag = &html[start..start + len];
        let name = tag_name(tag);
        at = start + len;
        if let Some(name) = name.strip_prefix('/') {
            // Close up to the element this ends (they nest properly).
            while let Some(index) = open.pop() {
                elements[index].close = start;
                elements[index].range.end = at;
                if elements[index].name == name {
                    break;
                }
            }
            continue;
        }
        let void = VOID.contains(&name);
        elements.push(Element {
            name,
            tag,
            range: start..at,
            close: at,
            parent: open.last().copied(),
        });
        if !void {
            open.push(elements.len() - 1);
        }
        if name == "style" {
            at = html[at..]
                .find("</style>")
                .map_or(html.len(), |end| at + end);
        }
    }
    for index in open {
        elements[index].close = html.len();
        elements[index].range.end = html.len();
    }
    elements
}

/// Whether `html` shows anything: text other than spaces, or an image.
fn significant(html: &str) -> bool {
    html.contains("<img")
        || text_content(html)
            .chars()
            .any(|c| !c.is_whitespace() && c != '\u{a0}')
}

/// The element just before `elements[index]` in the same parent, with only
/// blank space between them.
fn previous_sibling(html: &str, elements: &[Element<'_>], index: usize) -> Option<usize> {
    let element = &elements[index];
    (0..index)
        .rev()
        .find(|&i| {
            elements[i].parent == element.parent && elements[i].range.end <= element.range.start
        })
        .filter(|&i| !significant(&html[elements[i].range.end..element.range.start]))
}

/// Text that reads as a reply's attribution line.
fn attribution(text: &str) -> bool {
    let text = text.trim();
    text.len() < 400
        && ATTRIBUTION_ENDINGS
            .iter()
            .any(|ending| text.ends_with(ending))
}

/// How attribution lines end, in the languages Gmail and Apple Mail write
/// them most.
const ATTRIBUTION_ENDINGS: [&str; 8] = [
    "wrote:",
    "a écrit :",
    "a écrit:",
    "schrieb:",
    "escribió:",
    "scrisse:",
    "escreveu:",
    "schreef:",
];

/// Quoted text that is a forwarded message, which stays shown.
fn forwarded(text: &str) -> bool {
    let start: String = text.trim_start().chars().take(200).collect();
    start.contains("Forwarded message") || start.contains("Begin forwarded message")
}

/// The quoted history and signature in `html`, a message's cleaned HTML
/// whose IDs start with `id_prefix`.
pub fn find(html: &str, id_prefix: &str) -> Trimmed {
    let elements = elements(html);
    let quote = find_quote(html, &elements, id_prefix);
    let signature = find_signature(html, &elements, quote.as_ref());
    Trimmed { quote, signature }
}

fn find_quote(html: &str, elements: &[Element<'_>], id_prefix: &str) -> Option<Range<usize>> {
    let id_is = |element: &Element<'_>, id: &str| {
        attribute(element.tag, "id").is_some_and(|value| value == format!("{id_prefix}{id}"))
    };
    let mut candidates = Vec::new();
    for (index, element) in elements.iter().enumerate() {
        let sibling = || previous_sibling(html, elements, index).map(|i| &elements[i]);
        let range = if element.has_class("gmail_quote") || element.has_class("yahoo_quoted") {
            let start = sibling()
                .filter(|before| before.has_class("gmail_attr"))
                .map_or(element.range.start, |before| before.range.start);
            start..element.range.end
        } else if element.name == "blockquote"
            && attribute(element.tag, "type").as_deref() == Some("cite")
        {
            let start = sibling()
                .filter(|before| {
                    before.has_class("moz-cite-prefix")
                        || attribution(&text_content(&html[before.range.clone()]))
                })
                .map_or(element.range.start, |before| before.range.start);
            start..element.range.end
        } else if id_is(element, "appendonsend")
            || id_is(element, "divRplyFwdMsg")
            || id_is(element, "mail-editor-reference-message-container")
        {
            let start = sibling()
                .filter(|before| before.name == "hr")
                .map_or(element.range.start, |before| before.range.start);
            let end = element
                .parent
                .map_or(html.len(), |parent| elements[parent].close);
            start..end
        } else {
            continue;
        };
        candidates.push(range);
    }
    candidates
        .into_iter()
        .filter(|range| {
            !significant(&html[range.end..])
                && significant(&html[..range.start])
                && !forwarded(&text_content(&html[range.clone()]))
                && !interleaved(html, elements, range)
        })
        .min_by_key(|range| range.start)
}

/// Whether `range` holds a reply written between quotes: text of its own
/// after its first `<blockquote>`, outside the blockquotes.
fn interleaved(html: &str, elements: &[Element<'_>], range: &Range<usize>) -> bool {
    let mut quotes = elements.iter().filter(|element| {
        element.name == "blockquote"
            && element.range.start >= range.start
            && element.range.end <= range.end
    });
    let Some(first) = quotes.next() else {
        return false;
    };
    let mut at = first.range.end;
    for quote in quotes {
        if quote.range.start < at {
            // Inside the one before.
            continue;
        }
        if significant(&html[at..quote.range.start]) {
            return true;
        }
        at = quote.range.end;
    }
    significant(&html[at..range.end])
}

fn find_signature(
    html: &str,
    elements: &[Element<'_>],
    quote: Option<&Range<usize>>,
) -> Option<Signature> {
    let limit = quote.map_or(html.len(), |quote| quote.start);
    let (index, element) = elements.iter().enumerate().rev().find(|(_, element)| {
        element.range.end <= limit
            && (element.has_class("gmail_signature")
                || attribute(element.tag, "data-smartmail").as_deref() == Some("gmail_signature"))
    })?;
    // It ends the message, or comes right before the quote.
    if significant(&html[element.range.end..limit]) {
        return None;
    }
    // Gmail's "-- " before it, past the line break.
    let mut before = previous_sibling(html, elements, index);
    while let Some(i) = before.filter(|&i| elements[i].name == "br") {
        before = previous_sibling(html, elements, i);
    }
    let start = before
        .map(|i| &elements[i])
        .filter(|before| before.has_class("gmail_signature_prefix"))
        .map_or(element.range.start, |before| before.range.start);
    // A space for each tag, so lines and blocks stay apart.
    let text = normalize(&text_content(
        &html[element.range.clone()].replace('<', " <"),
    ));
    // The quote starts at an element (or the rule before it) with the
    // same parent.
    let joins_quote = quote.is_some_and(|quote| {
        elements
            .iter()
            .find(|other| other.range.start == quote.start)
            .is_some_and(|first| first.parent == element.parent)
    });
    (!text.is_empty() && significant(&html[..start])).then_some(Signature {
        range: start..element.range.end,
        text,
        joins_quote,
    })
}

/// `text` with runs of whitespace as one space, for comparing signatures.
pub fn normalize(text: &str) -> String {
    text.split(|c: char| c.is_whitespace() || c == '\u{a0}')
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// A plain-text body, split into the message's own text, its signature
/// and its quoted history.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextParts<'a> {
    pub body: &'a str,
    pub signature: Option<&'a str>,
    pub quote: Option<&'a str>,
}

/// The longest a plain-text signature runs, in lines.
const MAX_SIGNATURE_LINES: usize = 10;

/// Splits a plain-text body (see the module docs).
pub fn split_text(text: &str) -> TextParts<'_> {
    let lines: Vec<(usize, &str)> = line_starts(text);
    let quote_line = text_quote_start(&lines);
    let quote_at = quote_line.map_or(text.len(), |line| lines[line].0);
    // A signature: a "-- " line, and a few lines up to the quote or end.
    let before = &lines[..quote_line.unwrap_or(lines.len())];
    let signature_line = before
        .iter()
        .rposition(|(_, line)| matches!(line.trim_end_matches('\r'), "-- " | "--"))
        .filter(|&line| before.len() - line <= MAX_SIGNATURE_LINES + 1);
    let body_end = signature_line.map_or(quote_at, |line| lines[line].0);
    let body = text[..body_end].trim_end();
    if body.trim().is_empty() {
        // Nothing of its own: show it all.
        return TextParts {
            body: text,
            ..TextParts::default()
        };
    }
    TextParts {
        body,
        signature: signature_line
            .map(|line| text[lines[line].0..quote_at].trim_end())
            .filter(|signature| !signature.is_empty()),
        quote: quote_line.map(|_| text[quote_at..].trim_end()),
    }
}

/// Each line of `text` with the byte where it starts.
fn line_starts(text: &str) -> Vec<(usize, &str)> {
    let mut lines = Vec::new();
    let mut at = 0;
    for line in text.split('\n') {
        lines.push((at, line));
        at += line.len() + 1;
    }
    lines
}

/// The line the trailing quoted history of plain text starts on, if any.
fn text_quote_start(lines: &[(usize, &str)]) -> Option<usize> {
    let blank = |line: &str| line.trim().is_empty();
    // Outlook: a separator, then the headers of the message replied to.
    let separator = lines.iter().position(|(_, line)| {
        let line = line.trim();
        line == "-----Original Message-----" || (line.len() >= 20 && line.chars().all(|c| c == '_'))
    });
    let separator = separator.filter(|&at| {
        lines[at + 1..]
            .iter()
            .find(|(_, line)| !blank(line))
            .is_some_and(|(_, line)| line.trim_start().starts_with("From:"))
            || lines[at].1.trim() == "-----Original Message-----"
    });
    // A run of `>` lines that ends the text.
    let mut end = lines.len();
    while end > 0 && blank(lines[end - 1].1) {
        end -= 1;
    }
    let mut start = end;
    while start > 0 && (lines[start - 1].1.starts_with('>')) {
        start -= 1;
    }
    let quoted = (start < end).then(|| {
        // The attribution above it, maybe wrapped over two lines.
        let mut at = start;
        while at > 0 && blank(lines[at - 1].1) {
            at -= 1;
        }
        match at.checked_sub(1).map(|i| lines[i].1) {
            Some(line) if attribution(line) => {
                let first = at - 1;
                let wrapped = first > 0
                    && lines[first - 1].1.trim_start().starts_with("On ")
                    && !lines[first].1.trim_start().starts_with("On ");
                if wrapped { first - 1 } else { first }
            }
            _ => start,
        }
    });
    match (separator, quoted) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elements_nest_as_written() {
        let html =
            "<div class=\"a\"><p>x<br>y</p><img src=\"data:image/png;base64,AA==\"></div><p>z</p>";
        let elements = elements(html);
        let names: Vec<_> = elements.iter().map(|e| (e.name, e.parent)).collect();
        assert_eq!(
            names,
            [
                ("div", None),
                ("p", Some(0)),
                ("br", Some(1)),
                ("img", Some(0)),
                ("p", None)
            ]
        );
        assert_eq!(
            &html[elements[0].range.clone()],
            &html[..html.find("<p>z").unwrap()]
        );
        assert_eq!(&html[elements[4].range.clone()], "<p>z</p>");
    }

    #[test]
    fn plain_text_replies_split() {
        let parts = split_text(
            "Sounds good.\n\nDan\n\nOn Tue, Nov 10, 2026 at 3:07 PM Carol <c@example.com>\nwrote:\n\n> Hi all,\n>\n> Notes.\n",
        );
        assert_eq!(parts.body, "Sounds good.\n\nDan");
        assert_eq!(parts.signature, None);
        assert_eq!(
            parts.quote,
            Some(
                "On Tue, Nov 10, 2026 at 3:07 PM Carol <c@example.com>\nwrote:\n\n> Hi all,\n>\n> Notes."
            )
        );
    }

    #[test]
    fn plain_text_signatures_and_outlook_quotes() {
        let parts = split_text(
            "Thursday works.\n\n-- \nCarol Danvers\nMission control\n\n-----Original Message-----\nFrom: Dan\nSent: Tuesday\n\nCan we move it?\n",
        );
        assert_eq!(parts.body, "Thursday works.");
        assert_eq!(parts.signature, Some("-- \nCarol Danvers\nMission control"));
        assert_eq!(
            parts.quote,
            Some("-----Original Message-----\nFrom: Dan\nSent: Tuesday\n\nCan we move it?")
        );
    }

    #[test]
    fn inline_replies_and_bare_quotes_stay() {
        // Text after the quote: an inline reply.
        let inline = "On Mon, Ada wrote:\n> Lunch?\nYes!\n> Where?\nThe usual.";
        assert_eq!(split_text(inline).quote, None);
        // Only a quote: nothing of its own to show instead.
        let bare = "> just this\n> quoted";
        assert_eq!(split_text(bare).body, bare);
        assert_eq!(split_text(bare).quote, None);
        // A long sign-off isn't a signature.
        let long = format!("Hi\n-- \n{}", "line\n".repeat(20));
        assert_eq!(split_text(&long).signature, None);
    }
}
