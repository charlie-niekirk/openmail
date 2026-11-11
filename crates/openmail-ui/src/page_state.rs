//! What the reader did with the conversation document, kept across reloads.
//!
//! The document is loaded again when its content changes (images shown, a
//! new message, light or dark): a new page would start with the default
//! sections open and scrolled to the top. Instead, the webview runs one
//! trusted script of the app's own, [`PAGE_SCRIPT`] (email JavaScript stays
//! off), which reports a [`PageState`] as it changes: the sections open and
//! the place scrolled to. The next document of the same conversation
//! carries it back ([`with_page_state`]), and the script applies it before
//! the page is first painted.
//!
//! The reports come from a page showing untrusted mail, so they are parsed
//! strictly and bounded ([`PageState::parse`]).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::body_document::escape_html;

/// The script the webview injects into every document at its start.
pub const PAGE_SCRIPT: &str = include_str!("page_script.js");

/// The most sections a report may name; more means it isn't ours.
const MAX_SECTIONS: usize = 10_000;

/// The longest section ID a report may name.
const MAX_ID_LEN: usize = 256;

/// The sections open and the place scrolled to in a conversation document.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageState {
    /// Which document load reported it (see [`with_page_state`]), so a
    /// late report from the previous page is ignored.
    #[serde(default)]
    pub generation: u64,
    /// Whether each section (a message, or its quoted text) is open, by ID.
    #[serde(default)]
    pub open: BTreeMap<String, bool>,
    /// The first message in view, and how far into it the viewport starts,
    /// so the place holds when content above it changes size.
    #[serde(default)]
    pub anchor: Option<String>,
    #[serde(default)]
    pub offset: f64,
    /// The document's scroll position.
    #[serde(default)]
    pub x: f64,
    #[serde(default)]
    pub y: f64,
}

impl PageState {
    /// The state a page reported, or `None` if it isn't one: malformed,
    /// too large, or with IDs or numbers the documents never have.
    pub fn parse(report: &str) -> Option<Self> {
        // An object: serde would also take an array for a struct.
        if !report.trim_start().starts_with('{') {
            return None;
        }
        let state: Self = serde_json::from_str(report).ok()?;
        let valid = state.open.len() <= MAX_SECTIONS
            && state.open.keys().all(|id| valid_id(id))
            && state.anchor.as_deref().is_none_or(valid_id)
            && [state.offset, state.x, state.y]
                .iter()
                .all(|n| n.is_finite() && n.abs() < 1e7);
        valid.then_some(state)
    }
}

/// Whether `id` could be a section's ID (they are made of message IDs).
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// `document` (from [`conversation_document`](crate::conversation_document))
/// as loaded for the `generation`th time, carrying `state` from the
/// previous page of the same conversation, if any, for [`PAGE_SCRIPT`].
pub fn with_page_state(document: &str, generation: u64, state: Option<&PageState>) -> String {
    let state = PageState {
        generation,
        ..state.cloned().unwrap_or_default()
    };
    // Without a previous page, the place is left alone.
    let mut json = serde_json::to_value(&state).unwrap_or_default();
    if state
        == (PageState {
            generation,
            ..PageState::default()
        })
        && let Some(object) = json.as_object_mut()
    {
        object.retain(|key, _| key == "generation");
    }
    let meta = format!(
        "<meta name=\"om-state\" content=\"{}\">",
        escape_html(&json.to_string())
    );
    match document.find("<head>") {
        Some(at) => {
            let at = at + "<head>".len();
            format!("{}{meta}{}", &document[..at], &document[at..])
        }
        None => document.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_are_parsed() {
        let state = PageState::parse(
            r#"{"generation":3,"open":{"m-1a":true,"q-1a-0":false},
                "anchor":"m-1a","offset":12.5,"x":0,"y":480}"#,
        )
        .unwrap();
        assert_eq!(state.generation, 3);
        assert!(state.open["m-1a"]);
        assert!(!state.open["q-1a-0"]);
        assert_eq!(state.anchor.as_deref(), Some("m-1a"));
        assert_eq!((state.offset, state.x, state.y), (12.5, 0.0, 480.0));
        // Anchors may be missing (an empty document).
        let state = PageState::parse(r#"{"generation":1,"open":{},"anchor":null}"#).unwrap();
        assert_eq!(state.anchor, None);
    }

    #[test]
    fn odd_reports_are_ignored() {
        for report in [
            "",
            "null",
            "[]",
            "{\"open\":[]}",
            "{\"open\":{\"m-1\":\"yes\"}}",
            "{\"open\":{\"m-1\\\"><script>\":true}}",
            "{\"open\":{\"\":true}}",
            "{\"anchor\":\"a b\"}",
            "{\"y\":1e300}",
            "{\"x\":-1e8}",
        ] {
            assert_eq!(PageState::parse(report), None, "{report}");
        }
        let long = format!("{{\"anchor\":\"{}\"}}", "m".repeat(MAX_ID_LEN + 1));
        assert_eq!(PageState::parse(&long), None);
    }

    #[test]
    fn the_state_rides_in_the_head() {
        let document = "<!DOCTYPE html>\n<html><head><meta charset=\"utf-8\"></head><body>\
                        <p>&lt;head&gt;</p></body></html>";
        let state = PageState {
            generation: 1,
            open: BTreeMap::from([("m-1".to_owned(), false)]),
            anchor: Some("m-1".to_owned()),
            offset: 4.0,
            x: 0.0,
            y: 120.0,
        };
        assert_eq!(
            with_page_state(document, 7, Some(&state)),
            "<!DOCTYPE html>\n<html><head><meta name=\"om-state\" content=\"\
             {&quot;generation&quot;:7,&quot;open&quot;:{&quot;m-1&quot;:false},\
             &quot;anchor&quot;:&quot;m-1&quot;,&quot;offset&quot;:4.0,\
             &quot;x&quot;:0.0,&quot;y&quot;:120.0}\">\
             <meta charset=\"utf-8\"></head><body><p>&lt;head&gt;</p></body></html>"
        );
        // A first load only numbers the page.
        assert!(
            with_page_state(document, 2, None)
                .contains("<head><meta name=\"om-state\" content=\"{&quot;generation&quot;:2}\">")
        );
    }

    #[test]
    fn the_script_is_the_apps_own() {
        // It reads the state back under the names the app writes.
        for name in [
            "om-state",
            "generation",
            "open",
            "anchor",
            "offset",
            "details.om-message[id]",
            "details.om-quote[id]",
            "main.om-thread",
            "window.ipc.postMessage",
        ] {
            assert!(PAGE_SCRIPT.contains(name), "{name}");
        }
    }
}
