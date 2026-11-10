//! Small text helpers for terminal output.

use time::{OffsetDateTime, UtcOffset};

/// `2026-11-10 14:03` in `offset`.
pub fn format_date(date: OffsetDateTime, offset: UtcOffset) -> String {
    let date = date.to_offset(offset);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        date.year(),
        u8::from(date.month()),
        date.day(),
        date.hour(),
        date.minute()
    )
}

/// Shortens `s` to at most `max` characters, ending in `…` if it was cut.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.truncate(out.trim_end().len());
    out.push('…');
    out
}

/// Replaces runs of whitespace, including line breaks, with single spaces.
pub fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Decodes the HTML character references Gmail puts in snippets, such as
/// `&#39;` and `&amp;`. Unknown references are left as they are.
pub fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        let decoded = rest
            .find(';')
            .filter(|&end| end <= 10)
            .and_then(|end| decode_entity(&rest[1..end]).map(|c| (c, end)));
        match decoded {
            Some((c, end)) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn decode_entity(name: &str) -> Option<char> {
    match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some('\u{a0}'),
        _ => {
            let number = name.strip_prefix('#')?;
            let code = match number.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => number.parse().ok()?,
            };
            char::from_u32(code)
        }
    }
}

#[cfg(test)]
mod tests {
    use time::macros::{datetime, offset};

    use super::*;

    #[test]
    fn formats_dates_in_the_given_offset() {
        let date = datetime!(2026-11-10 23:30 UTC);
        assert_eq!(format_date(date, UtcOffset::UTC), "2026-11-10 23:30");
        assert_eq!(format_date(date, offset!(+1)), "2026-11-11 00:30");
        assert_eq!(format_date(date, offset!(-5:30)), "2026-11-10 18:00");
    }

    #[test]
    fn truncates_by_characters() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("exactly10!", 10), "exactly10!");
        assert_eq!(truncate("a longer sentence", 10), "a longer…");
        assert_eq!(truncate("héllo wörld", 6), "héllo…");
        assert_eq!(truncate("日本語のメール", 4), "日本語…");
    }

    #[test]
    fn collapses_whitespace() {
        assert_eq!(collapse_whitespace("  a\r\n\tb   c "), "a b c");
    }

    #[test]
    fn decodes_snippet_entities() {
        assert_eq!(
            decode_entities("Ada&#39;s &quot;plan&quot; &amp; notes &lt;3 &#x1F600;"),
            "Ada's \"plan\" & notes <3 😀"
        );
        assert_eq!(
            decode_entities("AT&T; R&D &bogus; &"),
            "AT&T; R&D &bogus; &"
        );
        assert_eq!(decode_entities("no entities"), "no entities");
    }
}
