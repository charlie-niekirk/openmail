//! Gmail labels: system and user labels, colors, visibility and nesting.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::LabelId;

/// Whether Gmail or the user owns a label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LabelType {
    /// Created by Gmail, such as `INBOX` or `CATEGORY_SOCIAL`.
    System,
    /// Created by the user.
    User,
}

/// A well-known Gmail system label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SystemLabel {
    Inbox,
    Sent,
    Draft,
    Spam,
    Trash,
    Starred,
    Important,
    Unread,
    Chat,
    CategoryPersonal,
    CategorySocial,
    CategoryPromotions,
    CategoryUpdates,
    CategoryForums,
}

impl SystemLabel {
    /// Every system label, in sidebar order.
    pub const ALL: [Self; 14] = [
        Self::Inbox,
        Self::Starred,
        Self::Important,
        Self::Sent,
        Self::Draft,
        Self::Spam,
        Self::Trash,
        Self::Unread,
        Self::Chat,
        Self::CategoryPersonal,
        Self::CategorySocial,
        Self::CategoryPromotions,
        Self::CategoryUpdates,
        Self::CategoryForums,
    ];

    /// The inbox category labels, in Gmail's tab order.
    pub const CATEGORIES: [Self; 5] = [
        Self::CategoryPersonal,
        Self::CategorySocial,
        Self::CategoryPromotions,
        Self::CategoryUpdates,
        Self::CategoryForums,
    ];

    /// Gmail's ID for this label, such as `"INBOX"`.
    pub const fn id_str(self) -> &'static str {
        match self {
            Self::Inbox => "INBOX",
            Self::Sent => "SENT",
            Self::Draft => "DRAFT",
            Self::Spam => "SPAM",
            Self::Trash => "TRASH",
            Self::Starred => "STARRED",
            Self::Important => "IMPORTANT",
            Self::Unread => "UNREAD",
            Self::Chat => "CHAT",
            Self::CategoryPersonal => "CATEGORY_PERSONAL",
            Self::CategorySocial => "CATEGORY_SOCIAL",
            Self::CategoryPromotions => "CATEGORY_PROMOTIONS",
            Self::CategoryUpdates => "CATEGORY_UPDATES",
            Self::CategoryForums => "CATEGORY_FORUMS",
        }
    }

    /// Gmail's ID for this label as a [`LabelId`].
    pub fn id(self) -> LabelId {
        LabelId::new(self.id_str())
    }

    /// Looks up a system label by its Gmail ID. Returns `None` for user labels
    /// and for system labels Openmail does not know about.
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|label| label.id_str() == id)
    }

    /// The name Gmail web shows for this label, such as "Drafts" or "Primary".
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Inbox => "Inbox",
            Self::Sent => "Sent",
            Self::Draft => "Drafts",
            Self::Spam => "Spam",
            Self::Trash => "Trash",
            Self::Starred => "Starred",
            Self::Important => "Important",
            Self::Unread => "Unread",
            Self::Chat => "Chats",
            Self::CategoryPersonal => "Primary",
            Self::CategorySocial => "Social",
            Self::CategoryPromotions => "Promotions",
            Self::CategoryUpdates => "Updates",
            Self::CategoryForums => "Forums",
        }
    }

    /// Whether this is one of the inbox category labels (`CATEGORY_*`).
    pub const fn is_category(self) -> bool {
        matches!(
            self,
            Self::CategoryPersonal
                | Self::CategorySocial
                | Self::CategoryPromotions
                | Self::CategoryUpdates
                | Self::CategoryForums
        )
    }
}

impl fmt::Display for SystemLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id_str())
    }
}

impl From<SystemLabel> for LabelId {
    fn from(label: SystemLabel) -> Self {
        label.id()
    }
}

impl LabelId {
    /// The system label this ID names, if it is a known one.
    pub fn system_label(&self) -> Option<SystemLabel> {
        SystemLabel::from_id(self.as_str())
    }

    /// Whether this ID names the given system label.
    pub fn is(&self, label: SystemLabel) -> bool {
        self.as_str() == label.id_str()
    }
}

/// A label's color in Gmail, as `#rrggbb` hex strings.
///
/// Gmail only accepts colors from a fixed palette; this type does not enforce it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LabelColor {
    /// Text color, such as `"#ffffff"`.
    pub text_color: String,
    /// Background color, such as `"#16a765"`.
    pub background_color: String,
}

/// Whether a label appears in the label list (the sidebar).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LabelListVisibility {
    /// Always shown.
    #[default]
    LabelShow,
    /// Shown only while it has unread mail.
    LabelShowIfUnread,
    /// Hidden.
    LabelHide,
}

/// Whether a label's chip appears on threads in the message list.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MessageListVisibility {
    /// Shown.
    #[default]
    Show,
    /// Hidden.
    Hide,
}

/// A Gmail label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Label {
    pub id: LabelId,
    /// The full name. User labels nest with `/`, as in `"Work/Projects"`.
    pub name: String,
    #[serde(rename = "type")]
    pub label_type: LabelType,
    pub color: Option<LabelColor>,
    pub label_list_visibility: LabelListVisibility,
    pub message_list_visibility: MessageListVisibility,
}

impl Label {
    /// Whether Gmail owns this label.
    pub fn is_system(&self) -> bool {
        self.label_type == LabelType::System
    }

    /// Whether the user created this label.
    pub fn is_user(&self) -> bool {
        self.label_type == LabelType::User
    }

    /// The well-known system label this is, if any.
    pub fn system_label(&self) -> Option<SystemLabel> {
        if self.is_system() {
            self.id.system_label()
        } else {
            None
        }
    }

    /// Whether this is one of the inbox category labels (`CATEGORY_*`).
    pub fn is_category(&self) -> bool {
        self.system_label().is_some_and(SystemLabel::is_category)
    }

    /// The label's name as a nested path.
    pub fn path(&self) -> LabelPath<'_> {
        LabelPath::new(&self.name)
    }
}

/// A label name viewed as a `/`-separated path, as Gmail nests user labels.
///
/// `"Work/Projects/Alpha"` is the label `Alpha`, nested in `Work/Projects`, which is
/// nested in `Work`. Gmail does not allow empty segments, so names are split on
/// every `/` as-is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LabelPath<'a>(&'a str);

/// The separator between nested label names.
pub const LABEL_PATH_SEPARATOR: char = '/';

impl<'a> LabelPath<'a> {
    /// Views a label name as a path.
    pub const fn new(name: &'a str) -> Self {
        Self(name)
    }

    /// The full name.
    pub const fn as_str(self) -> &'a str {
        self.0
    }

    /// The path's segments, outermost first: `"a/b/c"` gives `a`, `b`, `c`.
    pub fn segments(self) -> impl DoubleEndedIterator<Item = &'a str> {
        self.0.split(LABEL_PATH_SEPARATOR)
    }

    /// How deeply the label is nested: `0` for a top-level label.
    pub fn depth(self) -> usize {
        self.0.matches(LABEL_PATH_SEPARATOR).count()
    }

    /// The last segment, shown as the label's name in a tree: `"a/b/c"` gives `c`.
    pub fn leaf(self) -> &'a str {
        self.0
            .rsplit_once(LABEL_PATH_SEPARATOR)
            .map_or(self.0, |(_, leaf)| leaf)
    }

    /// The enclosing path, or `None` for a top-level label: `"a/b/c"` gives `a/b`.
    pub fn parent(self) -> Option<Self> {
        self.0
            .rsplit_once(LABEL_PATH_SEPARATOR)
            .map(|(parent, _)| Self(parent))
    }

    /// The enclosing paths, nearest first: `"a/b/c"` gives `a/b`, then `a`.
    pub fn ancestors(self) -> impl Iterator<Item = Self> {
        std::iter::successors(self.parent(), |path| path.parent())
    }

    /// Whether `self` encloses `other` at any depth. A path does not enclose itself.
    pub fn is_ancestor_of(self, other: LabelPath<'_>) -> bool {
        other
            .0
            .strip_prefix(self.0)
            .is_some_and(|rest| rest.starts_with(LABEL_PATH_SEPARATOR))
    }
}

impl fmt::Display for LabelPath<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label(id: &str, name: &str, label_type: LabelType) -> Label {
        Label {
            id: LabelId::new(id),
            name: name.to_owned(),
            label_type,
            color: None,
            label_list_visibility: LabelListVisibility::default(),
            message_list_visibility: MessageListVisibility::default(),
        }
    }

    #[test]
    fn system_label_ids_round_trip() {
        for system in SystemLabel::ALL {
            assert_eq!(SystemLabel::from_id(system.id_str()), Some(system));
            assert_eq!(system.id().system_label(), Some(system));
            assert!(LabelId::from(system).is(system));
        }
        assert_eq!(SystemLabel::from_id("Label_1"), None);
        assert_eq!(SystemLabel::from_id("inbox"), None);
    }

    #[test]
    fn classifies_system_and_user_labels() {
        let inbox = label("INBOX", "INBOX", LabelType::System);
        assert!(inbox.is_system());
        assert!(!inbox.is_user());
        assert_eq!(inbox.system_label(), Some(SystemLabel::Inbox));
        assert!(!inbox.is_category());

        let social = label("CATEGORY_SOCIAL", "CATEGORY_SOCIAL", LabelType::System);
        assert!(social.is_category());
        assert_eq!(
            social.system_label().map(SystemLabel::display_name),
            Some("Social")
        );

        let user = label("Label_7", "Receipts", LabelType::User);
        assert!(user.is_user());
        assert_eq!(user.system_label(), None);
        assert!(!user.is_category());

        // A user label is never treated as a system one, whatever its ID.
        let odd = label("INBOX", "INBOX", LabelType::User);
        assert_eq!(odd.system_label(), None);
    }

    #[test]
    fn categories_are_exactly_the_category_labels() {
        let categories: Vec<_> = SystemLabel::ALL
            .into_iter()
            .filter(|l| l.is_category())
            .collect();
        assert_eq!(categories, SystemLabel::CATEGORIES);
        assert!(
            SystemLabel::CATEGORIES
                .iter()
                .all(|l| l.id_str().starts_with("CATEGORY_"))
        );
    }

    #[test]
    fn label_path_splits_on_slash() {
        let path = LabelPath::new("Work/Projects/Alpha");
        assert_eq!(
            path.segments().collect::<Vec<_>>(),
            ["Work", "Projects", "Alpha"]
        );
        assert_eq!(path.depth(), 2);
        assert_eq!(path.leaf(), "Alpha");
        assert_eq!(path.parent(), Some(LabelPath::new("Work/Projects")));
        assert_eq!(
            path.ancestors().map(LabelPath::as_str).collect::<Vec<_>>(),
            ["Work/Projects", "Work"]
        );
    }

    #[test]
    fn top_level_label_path() {
        let path = LabelPath::new("Receipts");
        assert_eq!(path.segments().collect::<Vec<_>>(), ["Receipts"]);
        assert_eq!(path.depth(), 0);
        assert_eq!(path.leaf(), "Receipts");
        assert_eq!(path.parent(), None);
        assert_eq!(path.ancestors().count(), 0);
    }

    #[test]
    fn ancestry_respects_segment_boundaries() {
        let work = LabelPath::new("Work");
        assert!(work.is_ancestor_of(LabelPath::new("Work/Projects")));
        assert!(work.is_ancestor_of(LabelPath::new("Work/Projects/Alpha")));
        assert!(!work.is_ancestor_of(work));
        assert!(!work.is_ancestor_of(LabelPath::new("Workshop")));
        assert!(!work.is_ancestor_of(LabelPath::new("Home/Work")));
        assert!(!LabelPath::new("Work/Projects").is_ancestor_of(work));
    }

    #[test]
    fn label_exposes_its_path() {
        let user = label("Label_9", "Travel/2026/Japan", LabelType::User);
        assert_eq!(user.path().leaf(), "Japan");
        assert_eq!(user.path().depth(), 2);
    }

    #[test]
    fn visibility_serializes_with_gmail_names() {
        assert_eq!(
            serde_json::to_string(&LabelListVisibility::LabelShowIfUnread).unwrap(),
            r#""labelShowIfUnread""#
        );
        assert_eq!(
            serde_json::to_string(&MessageListVisibility::Hide).unwrap(),
            r#""hide""#
        );
        assert_eq!(
            serde_json::to_string(&LabelType::System).unwrap(),
            r#""system""#
        );
    }
}
