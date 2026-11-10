//! `labels`: every label with its message and unread counts.

use std::collections::HashSet;

use anyhow::Context as _;
use futures_util::{StreamExt as _, TryStreamExt as _, stream};
use openmail_core::{Label, LabelPath, SystemLabel};
use openmail_gmail::GmailClient;
use openmail_gmail::api::ApiLabel;

/// How many label requests run at once.
const CONCURRENCY: usize = 8;

/// Prints every label. `labels.list` has no counts, so each label is fetched.
pub async fn run(client: &GmailClient) -> anyhow::Result<()> {
    let labels = client
        .list_labels()
        .await
        .context("could not list labels")?;
    let labels: Vec<ApiLabel> = stream::iter(labels)
        .map(|label| {
            let client = client.clone();
            async move { client.get_label(&label.id).await }
        })
        .buffered(CONCURRENCY)
        .try_collect()
        .await
        .context("could not fetch label counts")?;
    print!("{}", format_labels(&labels));
    Ok(())
}

/// One line of the label table.
#[derive(Debug, PartialEq, Eq)]
struct Row<'a> {
    depth: usize,
    name: &'a str,
    messages: Option<u64>,
    unread: Option<u64>,
    id: &'a str,
}

/// Formats labels as a table: system labels in Gmail's sidebar order, then user
/// labels as a tree sorted by name, each nested label indented under its parent.
/// A parent missing from the list is shown without counts.
pub fn format_labels(labels: &[ApiLabel]) -> String {
    let mut system = Vec::new();
    let mut user = Vec::new();
    for api in labels {
        let label = Label::from(api.clone());
        if label.is_system() {
            system.push((api, label.system_label()));
        } else {
            user.push(api);
        }
    }

    system.sort_by_key(|(api, known)| {
        let order = known
            .and_then(|k| SystemLabel::ALL.iter().position(|&s| s == k))
            .unwrap_or(SystemLabel::ALL.len());
        (order, api.name.clone())
    });
    let system_rows: Vec<Row<'_>> = system
        .into_iter()
        .map(|(api, known)| Row {
            depth: 0,
            name: known.map_or(api.name.as_str(), |k| k.display_name()),
            messages: api.messages_total,
            unread: api.messages_unread,
            id: api.id.as_str(),
        })
        .collect();

    user.sort_by_cached_key(|api| tree_sort_key(&api.name));
    let mut user_rows = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for api in user {
        let path = LabelPath::new(&api.name);
        let mut ancestors: Vec<LabelPath<'_>> = path.ancestors().collect();
        ancestors.reverse();
        for ancestor in ancestors {
            if seen.insert(ancestor.as_str()) {
                user_rows.push(Row {
                    depth: ancestor.depth(),
                    name: ancestor.leaf(),
                    messages: None,
                    unread: None,
                    id: "",
                });
            }
        }
        if seen.insert(path.as_str()) {
            user_rows.push(Row {
                depth: path.depth(),
                name: path.leaf(),
                messages: api.messages_total,
                unread: api.messages_unread,
                id: api.id.as_str(),
            });
        }
    }

    render(&system_rows, &user_rows)
}

/// Sorts nested names depth first and ignoring case, so `a/b` follows `a` and
/// comes before `a b`.
fn tree_sort_key(name: &str) -> Vec<(String, String)> {
    LabelPath::new(name)
        .segments()
        .map(|s| (s.to_lowercase(), s.to_owned()))
        .collect()
}

fn render(system: &[Row<'_>], user: &[Row<'_>]) -> String {
    const INDENT: usize = 2;
    let label_width = system
        .iter()
        .chain(user)
        .map(|r| r.depth * INDENT + r.name.chars().count())
        .chain(["LABEL".len()])
        .max()
        .unwrap_or_default();
    let count = |n: Option<u64>| n.map_or_else(|| "-".to_owned(), |n| n.to_string());

    let mut out = String::new();
    let mut line = |label: &str, messages: &str, unread: &str, id: &str| {
        let line = format!("{label:<label_width$}  {messages:>8}  {unread:>6}  {id}");
        out.push_str(line.trim_end());
        out.push('\n');
    };
    line("LABEL", "MESSAGES", "UNREAD", "ID");
    for (i, rows) in [system, user].into_iter().enumerate() {
        if i > 0 && !rows.is_empty() && !system.is_empty() {
            line("", "", "", "");
        }
        for row in rows {
            let label = format!("{:indent$}{}", "", row.name, indent = row.depth * INDENT);
            line(&label, &count(row.messages), &count(row.unread), row.id);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use openmail_core::{LabelId, LabelType};

    use super::*;

    fn label(id: &str, name: &str, label_type: LabelType, counts: (u64, u64)) -> ApiLabel {
        ApiLabel {
            id: LabelId::new(id),
            name: name.to_owned(),
            label_type: Some(label_type),
            message_list_visibility: None,
            label_list_visibility: None,
            color: None,
            messages_total: Some(counts.0),
            messages_unread: Some(counts.1),
            threads_total: None,
            threads_unread: None,
        }
    }

    fn system(id: &str, counts: (u64, u64)) -> ApiLabel {
        label(id, id, LabelType::System, counts)
    }

    fn user(id: &str, name: &str, counts: (u64, u64)) -> ApiLabel {
        label(id, name, LabelType::User, counts)
    }

    #[test]
    fn system_labels_in_sidebar_order_then_user_labels_nested() {
        let labels = [
            user("Label_3", "Work/Projects/Alpha", (2, 2)),
            system("TRASH", (40, 0)),
            user("Label_1", "Work", (10, 1)),
            system("SENT", (300, 0)),
            system("INBOX", (1234, 12)),
            user("Label_4", "receipts", (7, 0)),
            user("Label_2", "Work/Projects", (5, 0)),
            system("CATEGORY_SOCIAL", (55, 3)),
        ];
        assert_eq!(
            format_labels(&labels),
            "\
LABEL       MESSAGES  UNREAD  ID
Inbox           1234      12  INBOX
Sent             300       0  SENT
Trash             40       0  TRASH
Social            55       3  CATEGORY_SOCIAL

receipts           7       0  Label_4
Work              10       1  Label_1
  Projects         5       0  Label_2
    Alpha          2       2  Label_3
"
        );
    }

    #[test]
    fn missing_parents_are_shown_without_counts() {
        let labels = [
            user("Label_9", "Clients/Acme/Invoices", (3, 1)),
            user("Label_8", "Clients/Acme", (4, 0)),
            user("Label_7", "Clients/Beta/Specs", (1, 1)),
        ];
        assert_eq!(
            format_labels(&labels),
            "\
LABEL         MESSAGES  UNREAD  ID
Clients              -       -
  Acme               4       0  Label_8
    Invoices         3       1  Label_9
  Beta               -       -
    Specs            1       1  Label_7
"
        );
    }

    #[test]
    fn nested_labels_sort_under_their_parent() {
        let labels = [
            user("Label_2", "a b", (0, 0)),
            user("Label_3", "a/b", (0, 0)),
            user("Label_1", "a", (0, 0)),
            user("Label_4", "B", (0, 0)),
        ];
        assert_eq!(
            format_labels(&labels),
            "\
LABEL  MESSAGES  UNREAD  ID
a             0       0  Label_1
  b           0       0  Label_3
a b           0       0  Label_2
B             0       0  Label_4
"
        );
    }

    #[test]
    fn unknown_system_labels_and_missing_counts() {
        let mut chat = system("CHAT", (0, 0));
        chat.messages_total = None;
        chat.messages_unread = None;
        let labels = [
            system("YELLOW_STAR", (1, 0)),
            chat,
            system("STARRED", (2, 0)),
        ];
        assert_eq!(
            format_labels(&labels),
            "\
LABEL        MESSAGES  UNREAD  ID
Starred             2       0  STARRED
Chats               -       -  CHAT
YELLOW_STAR         1       0  YELLOW_STAR
"
        );
    }

    #[test]
    fn no_labels_prints_only_the_header() {
        assert_eq!(format_labels(&[]), "LABEL  MESSAGES  UNREAD  ID\n");
    }
}
