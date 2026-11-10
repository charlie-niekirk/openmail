//! Subjects and threading headers for replies and forwards.

use openmail_core::MessageHeaders;

use crate::{OutgoingMessage, ParsedHeaders};

/// The subject of a reply: `Re: ` plus the parent's subject, unless it
/// already starts with `Re:` in any case.
pub fn reply_subject(subject: &str) -> String {
    prefixed("Re:", &["re:"], subject)
}

/// The subject of a forward: `Fwd: ` plus the parent's subject, unless it
/// already starts with `Fwd:` or `Fw:` in any case.
pub fn forward_subject(subject: &str) -> String {
    prefixed("Fwd:", &["fwd:", "fw:"], subject)
}

fn prefixed(prefix: &str, existing: &[&str], subject: &str) -> String {
    let subject = subject.trim();
    let has_prefix = existing.iter().any(|existing| {
        subject
            .get(..existing.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(existing))
    });
    if has_prefix {
        subject.to_owned()
    } else if subject.is_empty() {
        prefix.to_owned()
    } else {
        format!("{prefix} {subject}")
    }
}

/// The `In-Reply-To` and `References` of a reply or forward, computed from
/// its parent as RFC 5322 §3.6.4 describes. Message IDs are without angle
/// brackets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThreadingHeaders {
    /// The parent's `Message-ID`.
    pub in_reply_to: Option<String>,
    /// The parent's `References` (or its single `In-Reply-To` when it has no
    /// `References`), followed by the parent's `Message-ID`.
    pub references: Vec<String>,
}

impl ThreadingHeaders {
    /// Computes the headers from the parent's `Message-ID`, `In-Reply-To` and
    /// `References`. IDs may be given with or without angle brackets.
    pub fn for_parent(
        message_id: Option<&str>,
        in_reply_to: &[String],
        references: &[String],
    ) -> Self {
        let message_id = message_id
            .map(crate::parse::strip_angle_brackets)
            .filter(|id| !id.is_empty());
        let ancestors = match (references, in_reply_to) {
            ([], [single]) => std::slice::from_ref(single),
            (references, _) => references,
        };
        let mut chain: Vec<String> = Vec::with_capacity(ancestors.len() + 1);
        for id in ancestors
            .iter()
            .map(|id| crate::parse::strip_angle_brackets(id))
        {
            if !id.is_empty() && !chain.iter().any(|existing| existing == id) {
                chain.push(id.to_owned());
            }
        }
        if let Some(id) = message_id {
            chain.retain(|existing| existing != id);
            chain.push(id.to_owned());
        }
        Self {
            in_reply_to: message_id.map(str::to_owned),
            references: chain,
        }
    }

    /// The headers for replying to or forwarding a parsed message.
    pub fn from_parsed(parent: &ParsedHeaders) -> Self {
        Self::for_parent(
            parent.message_id.as_deref(),
            &parent.in_reply_to,
            &parent.references,
        )
    }

    /// The headers for replying to or forwarding a message known only by its
    /// metadata, whose ID headers are raw strings.
    pub fn from_core(parent: &MessageHeaders) -> Self {
        let ids = |raw: Option<&str>| raw.map(parse_message_ids).unwrap_or_default();
        let message_id = ids(parent.message_id_header.as_deref()).into_iter().next();
        Self::for_parent(
            message_id.as_deref(),
            &ids(parent.in_reply_to.as_deref()),
            &ids(parent.references.as_deref()),
        )
    }

    /// Sets the headers on an outgoing message.
    pub fn apply_to(self, message: &mut OutgoingMessage) {
        message.in_reply_to = self.in_reply_to;
        message.references = self.references;
    }
}

/// Splits a raw `Message-ID`, `In-Reply-To` or `References` header into
/// message IDs without angle brackets. Text outside angle brackets (such as
/// comments) is ignored; a header with no angle brackets at all is split on
/// whitespace and commas.
pub fn parse_message_ids(raw: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let mut rest = raw;
    while let Some(start) = rest.find('<') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('>') else {
            break;
        };
        let id = after[..end].trim();
        if !id.is_empty() {
            ids.push(id.to_owned());
        }
        rest = &after[end + 1..];
    }
    if ids.is_empty() {
        ids = raw
            .split(|c: char| c.is_whitespace() || c == ',')
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .collect();
    }
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|&id| id.to_owned()).collect()
    }

    #[test]
    fn reply_subject_adds_re_once() {
        assert_eq!(reply_subject("Lunch"), "Re: Lunch");
        assert_eq!(reply_subject("Re: Lunch"), "Re: Lunch");
        assert_eq!(reply_subject("RE: Lunch"), "RE: Lunch");
        assert_eq!(reply_subject("re:Lunch"), "re:Lunch");
        assert_eq!(reply_subject("  Lunch  "), "Re: Lunch");
        assert_eq!(reply_subject("Fwd: Lunch"), "Re: Fwd: Lunch");
        assert_eq!(reply_subject("Reports"), "Re: Reports");
        assert_eq!(reply_subject(""), "Re:");
        assert_eq!(reply_subject("Ré: café"), "Re: Ré: café");
    }

    #[test]
    fn forward_subject_adds_fwd_once() {
        assert_eq!(forward_subject("Lunch"), "Fwd: Lunch");
        assert_eq!(forward_subject("Fwd: Lunch"), "Fwd: Lunch");
        assert_eq!(forward_subject("FW: Lunch"), "FW: Lunch");
        assert_eq!(forward_subject("Re: Lunch"), "Fwd: Re: Lunch");
        assert_eq!(forward_subject("Fwiw"), "Fwd: Fwiw");
        assert_eq!(forward_subject(""), "Fwd:");
    }

    #[test]
    fn appends_the_parent_to_its_references() {
        let headers =
            ThreadingHeaders::for_parent(Some("<c@x>"), &ids(&["b@x"]), &ids(&["a@x", "<b@x>"]));
        assert_eq!(headers.in_reply_to.as_deref(), Some("c@x"));
        assert_eq!(headers.references, ["a@x", "b@x", "c@x"]);
    }

    #[test]
    fn falls_back_to_a_single_in_reply_to() {
        let headers = ThreadingHeaders::for_parent(Some("c@x"), &ids(&["b@x"]), &[]);
        assert_eq!(headers.references, ["b@x", "c@x"]);

        // Several In-Reply-To IDs are ambiguous, so none is used.
        let headers = ThreadingHeaders::for_parent(Some("c@x"), &ids(&["a@x", "b@x"]), &[]);
        assert_eq!(headers.references, ["c@x"]);
    }

    #[test]
    fn handles_a_parent_without_message_id_or_duplicates() {
        let headers = ThreadingHeaders::for_parent(None, &[], &ids(&["a@x", "b@x"]));
        assert_eq!(headers.in_reply_to, None);
        assert_eq!(headers.references, ["a@x", "b@x"]);

        let headers = ThreadingHeaders::for_parent(Some("a@x"), &[], &ids(&["a@x", "b@x", "b@x"]));
        assert_eq!(headers.references, ["b@x", "a@x"]);

        assert_eq!(
            ThreadingHeaders::for_parent(None, &[], &[]),
            ThreadingHeaders::default()
        );
    }

    #[test]
    fn reads_raw_core_headers() {
        let parent = MessageHeaders {
            message_id_header: Some("<c@x>".to_owned()),
            in_reply_to: Some("<b@x> (comment)".to_owned()),
            references: Some("<a@x>\r\n <b@x>".to_owned()),
            ..MessageHeaders::default()
        };
        let headers = ThreadingHeaders::from_core(&parent);
        assert_eq!(headers.in_reply_to.as_deref(), Some("c@x"));
        assert_eq!(headers.references, ["a@x", "b@x", "c@x"]);
    }

    #[test]
    fn parses_message_id_lists() {
        assert_eq!(parse_message_ids("<a@x> <b@x>"), ["a@x", "b@x"]);
        assert_eq!(parse_message_ids("<a@x>,<b@x> (c)"), ["a@x", "b@x"]);
        assert_eq!(parse_message_ids("a@x b@x"), ["a@x", "b@x"]);
        assert!(parse_message_ids("  ").is_empty());
    }
}
