//! Email addresses with optional display names, as in `From`, `To` and `Cc` headers.

use std::fmt::{self, Write as _};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// A mailbox: an email address with an optional display name.
///
/// Parses the forms found in decoded header values (RFC 5322 mailboxes, with UTF-8
/// allowed as in RFC 6532), such as `ada@example.com`, `Ada Lovelace
/// <ada@example.com>`, `"Lovelace, Ada" <ada@example.com>` and the older
/// `ada@example.com (Ada Lovelace)`. It does not decode RFC 2047 encoded words.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Address {
    /// The display name, such as `Ada Lovelace`. `None` when absent or empty.
    pub name: Option<String>,
    /// The address itself, such as `ada@example.com`.
    pub email: String,
}

impl Address {
    /// Creates an address with no display name, without validating it.
    pub fn new(email: impl Into<String>) -> Self {
        Self {
            name: None,
            email: email.into(),
        }
    }

    /// Creates an address with a display name, without validating it.
    pub fn named(name: impl Into<String>, email: impl Into<String>) -> Self {
        Self {
            name: Some(name.into()),
            email: email.into(),
        }
    }

    /// Parses a single mailbox, such as `Ada Lovelace <ada@example.com>`.
    pub fn parse(input: &str) -> Result<Self> {
        let invalid = || Error::InvalidAddress(input.to_owned());
        let s = input.trim();

        if let Some(open) = find_unquoted(s, '<') {
            let close = s[open..].find('>').ok_or_else(invalid)? + open;
            if !s[close + 1..].trim().is_empty() {
                return Err(invalid());
            }
            let email = s[open + 1..close].trim();
            if !is_valid_email(email) {
                return Err(invalid());
            }
            return Ok(Self {
                name: unquote_name(&s[..open]),
                email: email.to_owned(),
            });
        }

        // `ada@example.com (Ada Lovelace)`: a trailing comment holds the name.
        if let Some(open) = find_unquoted(s, '(') {
            let comment = s[open + 1..].strip_suffix(')').ok_or_else(invalid)?;
            let email = s[..open].trim();
            if !is_valid_email(email) {
                return Err(invalid());
            }
            return Ok(Self {
                name: unquote_name(comment),
                email: email.to_owned(),
            });
        }

        if !is_valid_email(s) {
            return Err(invalid());
        }
        Ok(Self {
            name: None,
            email: s.to_owned(),
        })
    }

    /// Parses a comma-separated address list, such as a `To` or `Cc` header value.
    ///
    /// Commas inside quoted names are not separators. Groups such as
    /// `Team: a@example.com, b@example.com;` contribute their members, and empty
    /// groups such as `undisclosed-recipients:;` contribute nothing.
    pub fn parse_list(input: &str) -> Result<Vec<Self>> {
        let mut addresses = Vec::new();
        let mut start = 0;
        let mut in_quotes = false;
        let mut escaped = false;
        let mut in_angle = false;
        let mut comment_depth = 0_u32;

        let mut push = |item: &str| -> Result<()> {
            if !item.trim().is_empty() {
                addresses.push(Self::parse(item)?);
            }
            Ok(())
        };

        for (i, c) in input.char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            match c {
                '\\' if in_quotes || comment_depth > 0 => escaped = true,
                '"' if comment_depth == 0 => in_quotes = !in_quotes,
                _ if in_quotes => {}
                '(' => comment_depth += 1,
                ')' => comment_depth = comment_depth.saturating_sub(1),
                _ if comment_depth > 0 => {}
                '<' => in_angle = true,
                '>' => in_angle = false,
                _ if in_angle => {}
                // The text before a group's colon is its name, not an address.
                ':' => start = i + 1,
                ',' | ';' => {
                    push(&input[start..i])?;
                    start = i + 1;
                }
                _ => {}
            }
        }
        push(&input[start..])?;
        Ok(addresses)
    }

    /// The display name if there is one, otherwise the email address.
    pub fn display_name_or_email(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.email)
    }
}

impl FromStr for Address {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        Self::parse(s)
    }
}

/// Formats as a mailbox, quoting the display name when it needs it:
/// `"Lovelace, Ada" <ada@example.com>`. Non-ASCII names are written as UTF-8,
/// not as RFC 2047 encoded words.
impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Some(name) = self.name.as_deref().filter(|n| !n.is_empty()) else {
            return f.write_str(&self.email);
        };
        if needs_quoting(name) {
            f.write_str("\"")?;
            for c in name.chars() {
                if matches!(c, '"' | '\\') {
                    f.write_str("\\")?;
                }
                f.write_char(c)?;
            }
            write!(f, "\" <{}>", self.email)
        } else {
            write!(f, "{name} <{}>", self.email)
        }
    }
}

/// The byte index of the first `target` outside a quoted string.
fn find_unquoted(s: &str, target: char) -> Option<usize> {
    let mut in_quotes = false;
    let mut escaped = false;
    for (i, c) in s.char_indices() {
        if escaped {
            escaped = false;
        } else if in_quotes && c == '\\' {
            escaped = true;
        } else if c == '"' {
            in_quotes = !in_quotes;
        } else if !in_quotes && c == target {
            return Some(i);
        }
    }
    None
}

/// Removes quoting and backslash escapes from a display name, trimming it.
fn unquote_name(raw: &str) -> Option<String> {
    let mut name = String::with_capacity(raw.len());
    let mut chars = raw.trim().chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {}
            '\\' => name.extend(chars.next()),
            _ => name.push(c),
        }
    }
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

/// A light check that `email` looks like `local@domain`, not full RFC 5322 validation.
fn is_valid_email(email: &str) -> bool {
    let Some((local, domain)) = email.rsplit_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && !email
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '<' | '>' | '(' | ')' | ',' | ';'))
}

/// Whether a display name must be quoted to be a valid RFC 5322 phrase.
fn needs_quoting(name: &str) -> bool {
    name != name.trim()
        || name.chars().any(|c| {
            matches!(
                c,
                '(' | ')' | '<' | '>' | '[' | ']' | ':' | ';' | '@' | '\\' | ',' | '.' | '"'
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(name: Option<&str>, email: &str) -> Address {
        name.map_or_else(|| Address::new(email), |name| Address::named(name, email))
    }

    #[test]
    fn parses_bare_address() {
        assert_eq!(
            Address::parse("  ada@example.com ").unwrap(),
            addr(None, "ada@example.com")
        );
        assert_eq!(
            Address::parse("<ada@example.com>").unwrap(),
            addr(None, "ada@example.com")
        );
    }

    #[test]
    fn parses_named_address() {
        assert_eq!(
            Address::parse("Ada Lovelace <ada@example.com>").unwrap(),
            addr(Some("Ada Lovelace"), "ada@example.com")
        );
        assert_eq!(
            Address::parse("Zoë Ångström<zoe@example.com>").unwrap(),
            addr(Some("Zoë Ångström"), "zoe@example.com")
        );
    }

    #[test]
    fn parses_quoted_name_with_specials_and_escapes() {
        assert_eq!(
            Address::parse(r#""Lovelace, Ada" <ada@example.com>"#).unwrap(),
            addr(Some("Lovelace, Ada"), "ada@example.com")
        );
        assert_eq!(
            Address::parse(r#""Ada \"The Countess\" <L>" <ada@example.com>"#).unwrap(),
            addr(Some(r#"Ada "The Countess" <L>"#), "ada@example.com")
        );
        assert_eq!(
            Address::parse(r#""" <ada@example.com>"#).unwrap(),
            addr(None, "ada@example.com")
        );
    }

    #[test]
    fn parses_comment_name() {
        assert_eq!(
            Address::parse("ada@example.com (Ada Lovelace)").unwrap(),
            addr(Some("Ada Lovelace"), "ada@example.com")
        );
    }

    #[test]
    fn rejects_invalid_addresses() {
        for input in [
            "",
            "Ada Lovelace",
            "ada",
            "@example.com",
            "ada@",
            "Ada <ada@example.com",
            "Ada <>",
            "Ada <ada@example.com> trailing",
            "ada lovelace@example.com",
        ] {
            assert_eq!(
                Address::parse(input),
                Err(Error::InvalidAddress(input.to_owned())),
                "{input:?}"
            );
        }
    }

    #[test]
    fn parses_lists_with_quoted_commas() {
        let list = Address::parse_list(
            r#"Ada <ada@example.com>, "Hopper, Grace" <grace@example.com>,alan@example.com"#,
        )
        .unwrap();
        assert_eq!(
            list,
            [
                addr(Some("Ada"), "ada@example.com"),
                addr(Some("Hopper, Grace"), "grace@example.com"),
                addr(None, "alan@example.com"),
            ]
        );
    }

    #[test]
    fn parses_list_edge_cases() {
        assert_eq!(Address::parse_list("").unwrap(), []);
        assert_eq!(Address::parse_list(" , ").unwrap(), []);
        assert_eq!(Address::parse_list("undisclosed-recipients:;").unwrap(), []);
        assert_eq!(
            Address::parse_list("Team: a@example.com, B <b@example.com>;, c@example.com").unwrap(),
            [
                addr(None, "a@example.com"),
                addr(Some("B"), "b@example.com"),
                addr(None, "c@example.com"),
            ]
        );
        assert_eq!(
            Address::parse_list("a@example.com (Smith, A), b@example.com").unwrap(),
            [
                addr(Some("Smith, A"), "a@example.com"),
                addr(None, "b@example.com"),
            ]
        );
        assert!(Address::parse_list("a@example.com, nobody").is_err());
    }

    #[test]
    fn formats_and_round_trips() {
        let cases = [
            (addr(None, "ada@example.com"), "ada@example.com"),
            (
                addr(Some("Ada Lovelace"), "ada@example.com"),
                "Ada Lovelace <ada@example.com>",
            ),
            (
                addr(Some("Lovelace, Ada"), "ada@example.com"),
                r#""Lovelace, Ada" <ada@example.com>"#,
            ),
            (
                addr(Some(r#"Ada "A.L." L"#), "ada@example.com"),
                r#""Ada \"A.L.\" L" <ada@example.com>"#,
            ),
        ];
        for (address, formatted) in cases {
            assert_eq!(address.to_string(), formatted);
            assert_eq!(formatted.parse::<Address>().unwrap(), address);
        }
    }

    #[test]
    fn display_name_falls_back_to_email() {
        assert_eq!(
            addr(Some("Ada"), "ada@example.com").display_name_or_email(),
            "Ada"
        );
        assert_eq!(
            addr(None, "ada@example.com").display_name_or_email(),
            "ada@example.com"
        );
    }
}
