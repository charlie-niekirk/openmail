//! The sidebar's view-model: the mailboxes in Gmail's order, then the user's
//! labels as a tree nested on `/`, with their colors, unread counts and which
//! nested labels are collapsed. [`Sidebar::rows`] flattens it into the rows the
//! view draws; the view is in [`sidebar_view`](crate::sidebar_view).

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use openmail_core::{Label, LabelColor, LabelId, LabelListVisibility, SystemLabel};

/// Gmail's ID for the Snoozed label, which only some accounts list.
pub const SNOOZED: &str = "SNOOZED";

/// A mailbox the sidebar can select.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Mailbox {
    /// Threads with this label.
    Label(LabelId),
    /// "All Mail": every thread outside Spam and Trash. It is not a label.
    AllMail,
}

impl Mailbox {
    /// The Inbox, selected by default.
    pub fn inbox() -> Self {
        Self::Label(SystemLabel::Inbox.id())
    }

    /// The label this mailbox shows, if it is one.
    pub fn label(&self) -> Option<&LabelId> {
        match self {
            Self::Label(id) => Some(id),
            Self::AllMail => None,
        }
    }
}

impl Default for Mailbox {
    fn default() -> Self {
        Self::inbox()
    }
}

/// Thread counts for one label.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Threads with the label.
    pub total: u32,
    /// Threads with the label and an unread message.
    pub unread: u32,
}

/// A part of the sidebar that collapses.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Group {
    /// The inbox categories (Social, Updates, Forums, Promotions).
    Categories,
    /// A user label's nested labels.
    Label(LabelId),
}

/// One row of the sidebar, top to bottom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// A mailbox to select.
    Mailbox(MailboxRow),
    /// The "Categories" row, which shows or hides the category mailboxes.
    Categories { expanded: bool },
    /// A section heading, such as "Labels".
    Heading(&'static str),
}

/// A row that selects a mailbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailboxRow {
    pub mailbox: Mailbox,
    /// The name shown: a nested label's name within its parent's.
    pub name: String,
    /// How far the row is indented: `0` at the top level.
    pub depth: usize,
    /// The count shown beside the name, if any (never zero).
    pub count: Option<u32>,
    /// Whether the count is of unread threads, which Gmail shows in bold.
    pub unread: bool,
    /// A user label's Gmail color.
    pub color: Option<LabelColor>,
    /// For a user label with nested labels, whether they are shown.
    pub expanded: Option<bool>,
    pub selected: bool,
}

/// Which count a mailbox shows, as Gmail web does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shown {
    Unread,
    Total,
    Nothing,
}

/// A system mailbox: the system label's ID (or none for All Mail), its name
/// and the count it shows.
struct System {
    id: Option<&'static str>,
    name: &'static str,
    shown: Shown,
    /// Shown only if the account lists the label.
    if_present: bool,
}

const fn system(label: SystemLabel, shown: Shown) -> System {
    System {
        id: Some(label.id_str()),
        name: label.display_name(),
        shown,
        if_present: false,
    }
}

/// The system mailboxes above the categories, in Gmail web's order.
const SYSTEM: [System; 9] = [
    system(SystemLabel::Inbox, Shown::Unread),
    system(SystemLabel::Starred, Shown::Nothing),
    System {
        id: Some(SNOOZED),
        name: "Snoozed",
        shown: Shown::Nothing,
        if_present: true,
    },
    system(SystemLabel::Important, Shown::Unread),
    system(SystemLabel::Sent, Shown::Nothing),
    system(SystemLabel::Draft, Shown::Total),
    System {
        id: None,
        name: "All Mail",
        shown: Shown::Nothing,
        if_present: false,
    },
    system(SystemLabel::Spam, Shown::Unread),
    system(SystemLabel::Trash, Shown::Nothing),
];

/// The categories Gmail web lists in its sidebar, in its order. Primary is
/// the rest of the Inbox and is not listed.
const CATEGORIES: [SystemLabel; 4] = [
    SystemLabel::CategorySocial,
    SystemLabel::CategoryUpdates,
    SystemLabel::CategoryForums,
    SystemLabel::CategoryPromotions,
];

/// The sidebar's state. Tests drive it directly; the view feeds it labels and
/// counts from the mail service.
#[derive(Debug, Clone)]
pub struct Sidebar {
    labels: Vec<Label>,
    counts: HashMap<LabelId, Counts>,
    collapsed: HashSet<Group>,
    selected: Mailbox,
}

impl Default for Sidebar {
    fn default() -> Self {
        Self::new()
    }
}

impl Sidebar {
    /// An empty sidebar with the Inbox selected and the categories collapsed.
    pub fn new() -> Self {
        Self {
            labels: Vec::new(),
            counts: HashMap::new(),
            collapsed: HashSet::from([Group::Categories]),
            selected: Mailbox::inbox(),
        }
    }

    /// Replaces the labels. If the selected label is gone, selects the Inbox
    /// and returns `true`.
    pub fn set_labels(&mut self, labels: Vec<Label>) -> bool {
        self.labels = labels;
        let exists = match &self.selected {
            Mailbox::AllMail => true,
            Mailbox::Label(id) => {
                SYSTEM
                    .iter()
                    .any(|system| !system.if_present && system.id == Some(id.as_str()))
                    || self.labels.iter().any(|label| &label.id == id)
            }
        };
        if !exists {
            self.selected = Mailbox::inbox();
        }
        self.collapsed.retain(|group| match group {
            Group::Categories => true,
            Group::Label(id) => self.labels.iter().any(|label| &label.id == id),
        });
        !exists
    }

    /// Replaces the counts. Labels missing from `counts` have no threads.
    pub fn set_counts(&mut self, counts: impl IntoIterator<Item = (LabelId, Counts)>) {
        self.counts = counts.into_iter().collect();
    }

    /// A label's counts.
    pub fn counts(&self, id: &LabelId) -> Counts {
        self.counts.get(id).copied().unwrap_or_default()
    }

    /// The selected mailbox.
    pub fn selected(&self) -> &Mailbox {
        &self.selected
    }

    /// Selects a mailbox. Returns whether the selection changed.
    pub fn select(&mut self, mailbox: Mailbox) -> bool {
        if self.selected == mailbox {
            return false;
        }
        self.selected = mailbox;
        true
    }

    /// Whether a group is collapsed.
    pub fn is_collapsed(&self, group: &Group) -> bool {
        self.collapsed.contains(group)
    }

    /// Collapses an expanded group, or expands a collapsed one.
    pub fn toggle(&mut self, group: Group) {
        if !self.collapsed.remove(&group) {
            self.collapsed.insert(group);
        }
    }

    /// The rows to show, top to bottom.
    pub fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        for system in &SYSTEM {
            let mailbox = match system.id {
                Some(id) => {
                    if system.if_present && !self.labels.iter().any(|l| l.id.as_str() == id) {
                        continue;
                    }
                    Mailbox::Label(LabelId::new(id))
                }
                None => Mailbox::AllMail,
            };
            rows.push(Row::Mailbox(self.mailbox_row(
                mailbox,
                system.name.to_owned(),
                0,
                system.shown,
            )));
        }

        let categories: Vec<_> = CATEGORIES
            .into_iter()
            .filter(|category| self.labels.iter().any(|l| l.id.is(*category)))
            .collect();
        if !categories.is_empty() {
            let expanded = !self.is_collapsed(&Group::Categories);
            rows.push(Row::Categories { expanded });
            if expanded {
                rows.extend(categories.into_iter().map(|category| {
                    Row::Mailbox(self.mailbox_row(
                        Mailbox::Label(category.id()),
                        category.display_name().to_owned(),
                        1,
                        Shown::Unread,
                    ))
                }));
            }
        }

        let tree = self.user_tree();
        if !tree.roots.is_empty() {
            rows.push(Row::Heading("Labels"));
            for root in &tree.roots {
                self.push_user_label(&tree, root, None, 0, &mut rows);
            }
        }
        rows
    }

    fn mailbox_row(
        &self,
        mailbox: Mailbox,
        name: String,
        depth: usize,
        shown: Shown,
    ) -> MailboxRow {
        let counts = mailbox
            .label()
            .map(|id| self.counts(id))
            .unwrap_or_default();
        let count = match shown {
            Shown::Unread => counts.unread,
            Shown::Total => counts.total,
            Shown::Nothing => 0,
        };
        MailboxRow {
            selected: self.selected == mailbox,
            mailbox,
            name,
            depth,
            count: (count > 0).then_some(count),
            unread: shown == Shown::Unread && count > 0,
            color: None,
            expanded: None,
        }
    }

    /// Whether a user label is listed: Gmail's label-list visibility, except
    /// that the selected label always is.
    fn is_listed(&self, label: &Label) -> bool {
        let visible = match label.label_list_visibility {
            LabelListVisibility::LabelShow => true,
            LabelListVisibility::LabelShowIfUnread => self.counts(&label.id).unread > 0,
            LabelListVisibility::LabelHide => false,
        };
        visible || self.selected.label() == Some(&label.id)
    }

    /// The listed user labels as a tree. A label nests in its nearest listed
    /// ancestor; without one it is a root, named by its full path.
    fn user_tree(&self) -> Tree<'_> {
        let listed: HashMap<&str, &Label> = self
            .labels
            .iter()
            .filter(|label| label.is_user() && self.is_listed(label))
            .map(|label| (label.name.as_str(), label))
            .collect();

        let mut tree = Tree::default();
        for label in listed.values() {
            let parent = label
                .path()
                .ancestors()
                .find_map(|ancestor| listed.get(ancestor.as_str()).copied());
            match parent {
                Some(parent) => tree.children.entry(&parent.id).or_default().push(label),
                None => tree.roots.push(label),
            }
        }
        let by_name = |a: &&Label, b: &&Label| compare_names(&a.name, &b.name);
        tree.roots.sort_by(by_name);
        for children in tree.children.values_mut() {
            children.sort_by(by_name);
        }
        tree
    }

    fn push_user_label(
        &self,
        tree: &Tree<'_>,
        label: &Label,
        parent: Option<&Label>,
        depth: usize,
        rows: &mut Vec<Row>,
    ) {
        let name = match parent {
            Some(parent) => label.name[parent.name.len() + 1..].to_owned(),
            None => label.name.clone(),
        };
        let children = tree.children.get(&label.id);
        let expanded = !self.is_collapsed(&Group::Label(label.id.clone()));
        let mut row =
            self.mailbox_row(Mailbox::Label(label.id.clone()), name, depth, Shown::Unread);
        row.color = label.color.clone();
        row.expanded = children.map(|_| expanded);
        rows.push(Row::Mailbox(row));

        if let Some(children) = children
            && expanded
        {
            for child in children {
                self.push_user_label(tree, child, Some(label), depth + 1, rows);
            }
        }
    }
}

/// The listed user labels, nested.
#[derive(Default)]
struct Tree<'a> {
    roots: Vec<&'a Label>,
    children: HashMap<&'a LabelId, Vec<&'a Label>>,
}

/// Gmail's label order: alphabetical, ignoring case.
fn compare_names(a: &str, b: &str) -> Ordering {
    a.to_lowercase()
        .cmp(&b.to_lowercase())
        .then_with(|| a.cmp(b))
}

/// Parses a Gmail label color such as `"#16a765"` into `0xrrggbb`.
pub fn parse_hex_color(color: &str) -> Option<u32> {
    let hex = color.strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(hex, 16).ok()
}

#[cfg(test)]
mod tests {
    use openmail_core::{LabelType, MessageListVisibility};

    use super::*;

    fn label(id: &str, name: &str, label_type: LabelType) -> Label {
        Label {
            id: LabelId::new(id),
            name: name.to_owned(),
            label_type,
            color: None,
            label_list_visibility: LabelListVisibility::LabelShow,
            message_list_visibility: MessageListVisibility::Show,
        }
    }

    fn system(id: &str) -> Label {
        label(id, id, LabelType::System)
    }

    fn user(id: &str, name: &str) -> Label {
        label(id, name, LabelType::User)
    }

    /// Every system label Gmail lists for an account with categories on.
    fn system_labels() -> Vec<Label> {
        SystemLabel::ALL
            .into_iter()
            .map(|label| system(label.id_str()))
            .collect()
    }

    fn counts(id: &str, total: u32, unread: u32) -> (LabelId, Counts) {
        (LabelId::new(id), Counts { total, unread })
    }

    /// The rows as text: indentation, then the name, then the count.
    fn render(sidebar: &Sidebar) -> Vec<String> {
        sidebar
            .rows()
            .into_iter()
            .map(|row| match row {
                Row::Mailbox(row) => {
                    let mut text = format!("{}{}", "  ".repeat(row.depth), row.name);
                    if let Some(expanded) = row.expanded {
                        text.push_str(if expanded { " [-]" } else { " [+]" });
                    }
                    if let Some(count) = row.count {
                        text.push_str(&format!(" {count}"));
                    }
                    if row.selected {
                        text.push_str(" *");
                    }
                    text
                }
                Row::Categories { expanded: true } => "Categories [-]".to_owned(),
                Row::Categories { expanded: false } => "Categories [+]".to_owned(),
                Row::Heading(heading) => format!("# {heading}"),
            })
            .collect()
    }

    fn mailbox_row(sidebar: &Sidebar, name: &str) -> MailboxRow {
        sidebar
            .rows()
            .into_iter()
            .find_map(|row| match row {
                Row::Mailbox(row) if row.name == name => Some(row),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no row named {name}"))
    }

    #[test]
    fn lists_system_mailboxes_in_gmail_order() {
        let sidebar = Sidebar::new();
        assert_eq!(
            render(&sidebar),
            [
                "Inbox *",
                "Starred",
                "Important",
                "Sent",
                "Drafts",
                "All Mail",
                "Spam",
                "Trash",
            ]
        );
    }

    #[test]
    fn lists_snoozed_and_categories_when_present() {
        let mut sidebar = Sidebar::new();
        let mut labels = system_labels();
        labels.push(system(SNOOZED));
        sidebar.set_labels(labels);
        assert_eq!(
            render(&sidebar),
            [
                "Inbox *",
                "Starred",
                "Snoozed",
                "Important",
                "Sent",
                "Drafts",
                "All Mail",
                "Spam",
                "Trash",
                "Categories [+]",
            ]
        );

        sidebar.toggle(Group::Categories);
        assert!(!sidebar.is_collapsed(&Group::Categories));
        assert_eq!(
            render(&sidebar)[9..],
            [
                "Categories [-]",
                "  Social",
                "  Updates",
                "  Forums",
                "  Promotions",
            ]
        );
    }

    #[test]
    fn shows_the_counts_gmail_shows() {
        let mut sidebar = Sidebar::new();
        sidebar.set_labels(system_labels());
        sidebar.toggle(Group::Categories);
        sidebar.set_counts([
            counts("INBOX", 120, 7),
            counts("STARRED", 4, 1),
            counts("IMPORTANT", 30, 2),
            counts("SENT", 300, 0),
            counts("DRAFT", 3, 0),
            counts("SPAM", 12, 5),
            counts("TRASH", 9, 9),
            counts("CATEGORY_SOCIAL", 40, 11),
            counts("CATEGORY_UPDATES", 50, 0),
        ]);
        assert_eq!(
            render(&sidebar),
            [
                "Inbox 7 *",
                "Starred",
                "Important 2",
                "Sent",
                "Drafts 3",
                "All Mail",
                "Spam 5",
                "Trash",
                "Categories [-]",
                "  Social 11",
                "  Updates",
                "  Forums",
                "  Promotions",
            ]
        );
        assert!(mailbox_row(&sidebar, "Inbox").unread);
        // Drafts counts every draft, which is not unread mail.
        assert!(!mailbox_row(&sidebar, "Drafts").unread);
        assert_eq!(sidebar.counts(&LabelId::new("SENT")).total, 300);
        assert_eq!(sidebar.counts(&LabelId::new("Label_9")), Counts::default());
    }

    #[test]
    fn nests_user_labels_on_slashes() {
        let mut sidebar = Sidebar::new();
        sidebar.set_labels(vec![
            user("Label_1", "Work"),
            user("Label_2", "Work/Projects/Alpha"),
            user("Label_3", "receipts"),
            user("Label_4", "Work/Projects"),
            user("Label_5", "Travel/Japan"),
            user("Label_6", "Home"),
            user("Label_7", "Work/Admin"),
        ]);
        sidebar.set_counts([counts("Label_2", 8, 3), counts("Label_3", 2, 0)]);
        assert_eq!(
            render(&sidebar)[8..],
            [
                "# Labels",
                "Home",
                "receipts",
                // Its parent is not a label, so it is listed by its full name.
                "Travel/Japan",
                "Work [-]",
                "  Admin",
                "  Projects [-]",
                "    Alpha 3",
            ]
        );
        assert!(mailbox_row(&sidebar, "Alpha").unread);
    }

    #[test]
    fn collapses_nested_labels() {
        let mut sidebar = Sidebar::new();
        sidebar.set_labels(vec![
            user("Label_1", "Work"),
            user("Label_2", "Work/Projects"),
            user("Label_3", "Work/Projects/Alpha"),
            user("Label_4", "Zoo"),
        ]);
        let work = Group::Label(LabelId::new("Label_1"));
        sidebar.toggle(Group::Label(LabelId::new("Label_2")));
        assert_eq!(
            render(&sidebar)[8..],
            ["# Labels", "Work [-]", "  Projects [+]", "Zoo"]
        );

        sidebar.toggle(work.clone());
        assert!(sidebar.is_collapsed(&work));
        assert_eq!(render(&sidebar)[8..], ["# Labels", "Work [+]", "Zoo"]);

        // Expanding a parent keeps its children's own state.
        sidebar.toggle(work);
        assert_eq!(
            render(&sidebar)[8..],
            ["# Labels", "Work [-]", "  Projects [+]", "Zoo"]
        );

        // Collapse state is dropped with its label.
        sidebar.set_labels(vec![user("Label_1", "Work")]);
        assert!(!sidebar.is_collapsed(&Group::Label(LabelId::new("Label_2"))));
    }

    #[test]
    fn follows_label_list_visibility() {
        let mut hidden = user("Label_1", "Hidden");
        hidden.label_list_visibility = LabelListVisibility::LabelHide;
        let mut if_unread = user("Label_2", "Newsletters");
        if_unread.label_list_visibility = LabelListVisibility::LabelShowIfUnread;
        let mut hidden_parent = user("Label_3", "Archive");
        hidden_parent.label_list_visibility = LabelListVisibility::LabelHide;
        let child = user("Label_4", "Archive/2025");

        let mut sidebar = Sidebar::new();
        sidebar.set_labels(vec![hidden, if_unread, hidden_parent, child]);
        assert_eq!(render(&sidebar)[8..], ["# Labels", "Archive/2025"]);

        sidebar.set_counts([counts("Label_2", 5, 1)]);
        assert_eq!(
            render(&sidebar)[8..],
            ["# Labels", "Archive/2025", "Newsletters 1"]
        );

        // A selected label stays listed after it is read.
        sidebar.select(Mailbox::Label(LabelId::new("Label_2")));
        sidebar.set_counts([counts("Label_2", 5, 0)]);
        assert_eq!(
            render(&sidebar)[8..],
            ["# Labels", "Archive/2025", "Newsletters *"]
        );
    }

    #[test]
    fn hides_system_labels_from_the_label_tree() {
        let mut sidebar = Sidebar::new();
        sidebar.set_labels(system_labels());
        assert!(!render(&sidebar).iter().any(|row| row.contains("Labels")));
        assert!(!render(&sidebar).iter().any(|row| row.contains("UNREAD")));
    }

    #[test]
    fn selects_the_inbox_by_default_and_tracks_the_selection() {
        let mut sidebar = Sidebar::new();
        assert_eq!(sidebar.selected(), &Mailbox::inbox());
        assert!(mailbox_row(&sidebar, "Inbox").selected);

        assert!(sidebar.select(Mailbox::AllMail));
        assert!(!sidebar.select(Mailbox::AllMail));
        assert!(mailbox_row(&sidebar, "All Mail").selected);
        assert!(!mailbox_row(&sidebar, "Inbox").selected);
        assert_eq!(sidebar.selected().label(), None);
    }

    #[test]
    fn falls_back_to_the_inbox_when_the_selected_label_goes() {
        let mut sidebar = Sidebar::new();
        sidebar.set_labels(vec![user("Label_1", "Work")]);
        let work = Mailbox::Label(LabelId::new("Label_1"));
        sidebar.select(work.clone());
        assert!(!sidebar.set_labels(vec![user("Label_1", "Work (renamed)")]));
        assert_eq!(sidebar.selected(), &work);

        assert!(sidebar.set_labels(Vec::new()));
        assert_eq!(sidebar.selected(), &Mailbox::inbox());

        // Fixed mailboxes stay selected whatever the label list says.
        sidebar.select(Mailbox::Label(SystemLabel::Trash.id()));
        assert!(!sidebar.set_labels(Vec::new()));
        sidebar.select(Mailbox::AllMail);
        assert!(!sidebar.set_labels(Vec::new()));

        // Snoozed only while the account lists it.
        sidebar.set_labels(vec![system(SNOOZED)]);
        sidebar.select(Mailbox::Label(LabelId::new(SNOOZED)));
        assert!(sidebar.set_labels(Vec::new()));
        assert_eq!(sidebar.selected(), &Mailbox::inbox());
    }

    #[test]
    fn keeps_user_label_colors() {
        let mut work = user("Label_1", "Work");
        let color = LabelColor {
            text_color: "#ffffff".to_owned(),
            background_color: "#16a765".to_owned(),
        };
        work.color = Some(color.clone());
        let mut sidebar = Sidebar::new();
        sidebar.set_labels(vec![work]);
        assert_eq!(mailbox_row(&sidebar, "Work").color, Some(color));
        assert_eq!(mailbox_row(&sidebar, "Inbox").color, None);
    }

    #[test]
    fn parses_gmail_colors() {
        assert_eq!(parse_hex_color("#16a765"), Some(0x16a765));
        assert_eq!(parse_hex_color("#FFFFFF"), Some(0xffffff));
        assert_eq!(parse_hex_color("16a765"), None);
        assert_eq!(parse_hex_color("#fff"), None);
        assert_eq!(parse_hex_color("#zzzzzz"), None);
        assert_eq!(parse_hex_color("#+12345"), None);
    }
}
