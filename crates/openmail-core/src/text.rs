//! Text helpers for strings Gmail sends.

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
    use super::*;

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
