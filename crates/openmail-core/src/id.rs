//! Strongly typed identifiers.
//!
//! Gmail identifies labels, threads, messages, drafts and attachments with opaque
//! strings. Wrapping each in its own type stops a `ThreadId` being passed where a
//! `MessageId` is expected. History IDs are the exception: Gmail sends them as
//! strings, but they are unsigned integers that increase over time, so
//! [`HistoryId`] wraps a `u64` and can be compared.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::Error;

macro_rules! string_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Wraps an ID string.
            pub fn new(id: impl Into<String>) -> Self {
                Self(id.into())
            }

            /// Returns the ID as a string slice.
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// Returns the owned ID string.
            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl From<String> for $name {
            fn from(id: String) -> Self {
                Self(id)
            }
        }

        impl From<&str> for $name {
            fn from(id: &str) -> Self {
                Self(id.to_owned())
            }
        }
    };
}

string_id! {
    /// Identifies a local Openmail account (one signed-in Google account).
    AccountId
}

string_id! {
    /// A Gmail label ID, such as `INBOX` or `Label_42`.
    LabelId
}

string_id! {
    /// A Gmail thread ID.
    ThreadId
}

string_id! {
    /// A Gmail message ID (not the RFC 5322 `Message-ID` header).
    MessageId
}

string_id! {
    /// A Gmail draft ID.
    DraftId
}

string_id! {
    /// A Gmail attachment ID, used to fetch an attachment body.
    ///
    /// Gmail may return a different attachment ID for the same part on each fetch,
    /// so identify a stored attachment by its message and part ID instead.
    AttachmentId
}

/// A Gmail history ID: a point in a mailbox's change history.
///
/// History IDs increase over time, so the newest one seen is the starting point
/// for the next `history.list` call. Serialized as a decimal string, as Gmail does;
/// deserializes from a string or a number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HistoryId(u64);

impl HistoryId {
    /// Wraps a numeric history ID.
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    /// Returns the numeric value.
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl From<u64> for HistoryId {
    fn from(id: u64) -> Self {
        Self(id)
    }
}

impl fmt::Display for HistoryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for HistoryId {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse()
            .map(Self)
            .map_err(|_| Error::InvalidHistoryId(s.to_owned()))
    }
}

impl Serialize for HistoryId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for HistoryId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl de::Visitor<'_> for Visitor {
            type Value = HistoryId;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a history ID as a decimal string or unsigned integer")
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<HistoryId, E> {
                Ok(HistoryId(v))
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<HistoryId, E> {
                v.parse().map_err(E::custom)
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_ids_serialize_transparently() {
        let id = ThreadId::new("18c0ffee");
        assert_eq!(serde_json::to_string(&id).unwrap(), r#""18c0ffee""#);
        let back: ThreadId = serde_json::from_str(r#""18c0ffee""#).unwrap();
        assert_eq!(back, id);
        assert_eq!(id.to_string(), "18c0ffee");
    }

    #[test]
    fn history_id_parses_and_orders_numerically() {
        let a: HistoryId = "9".parse().unwrap();
        let b: HistoryId = "10".parse().unwrap();
        assert!(a < b);
        assert_eq!(
            "x1".parse::<HistoryId>(),
            Err(Error::InvalidHistoryId("x1".to_owned()))
        );
        assert!("".parse::<HistoryId>().is_err());
        assert!("-1".parse::<HistoryId>().is_err());
    }

    #[test]
    fn history_id_serde_uses_strings_and_accepts_numbers() {
        let id = HistoryId::new(123_456);
        assert_eq!(serde_json::to_string(&id).unwrap(), r#""123456""#);
        assert_eq!(
            serde_json::from_str::<HistoryId>(r#""123456""#).unwrap(),
            id
        );
        assert_eq!(serde_json::from_str::<HistoryId>("123456").unwrap(), id);
        assert!(serde_json::from_str::<HistoryId>(r#""abc""#).is_err());
    }
}
