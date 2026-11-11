//! The conversation view-model: an open thread's header and its messages,
//! collapsed or expanded the way Gmail shows them, with each expanded
//! message's body loaded lazily.
//!
//! [`Conversation`] has no GPUI types; `conversation_view` renders it and
//! drives its loading.

use std::collections::HashMap;

use openmail_core::{Address, Label, LabelColor, LabelId, MessageId, SystemLabel, ThreadId};
use openmail_store::{MessageBody, ThreadDetail};
use time::OffsetDateTime;

/// What a thread with no subject is called.
pub const NO_SUBJECT: &str = "(no subject)";

/// What an expanded message shows when its body has no text at all.
pub const EMPTY_BODY: &str = "(This message has no text.)";

/// An open thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conversation {
    pub thread_id: ThreadId,
    pub subject: String,
    /// The label chips beside the subject.
    pub labels: Vec<LabelChip>,
    /// The messages, oldest first.
    pub messages: Vec<MessageItem>,
}

/// A label chip in the conversation header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelChip {
    pub id: LabelId,
    /// The name Gmail shows: "Inbox" for system labels, the full path (such
    /// as "Work/Projects") for user labels.
    pub name: String,
    /// The label's colors, if the user set any.
    pub color: Option<LabelColor>,
}

/// One message in a [`Conversation`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageItem {
    pub id: MessageId,
    /// Who sent it, as short as possible: the display name, or "me".
    pub sender: String,
    /// The full `From`, as in `Ada Lovelace <ada@example.com>`.
    pub from: String,
    /// The full `To` and `Cc` lists (empty if there are none).
    pub to: String,
    pub cc: String,
    /// The recipients in short, as in "to me, Bob Smith".
    pub recipients: String,
    pub date: OffsetDateTime,
    /// Gmail's plain-text excerpt, entities decoded.
    pub snippet: String,
    pub unread: bool,
    /// Whether the message shows its participants and body, or only a line.
    pub expanded: bool,
    pub body: BodyState,
}

/// Where an expanded message's body is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyState {
    /// Not requested yet (collapsed messages don't ask).
    NotLoaded,
    /// Requested from the mail service.
    Loading,
    /// The text to show.
    Loaded(String),
    /// The message is gone, or its body could not be read or fetched.
    Unavailable(String),
}

impl Conversation {
    /// Builds the conversation for `detail`. `labels` is the account's label
    /// list (for chip names and colors) and `me` the account's address.
    ///
    /// Unread messages and the latest one start expanded; the rest are
    /// collapsed to a line.
    pub fn new(detail: ThreadDetail, labels: &[Label], me: Option<&str>) -> Self {
        let last = detail.messages.len().saturating_sub(1);
        let subject = detail
            .thread
            .subject
            .clone()
            .or_else(|| {
                detail
                    .messages
                    .first()
                    .and_then(|first| first.message.headers.subject.clone())
            })
            .filter(|subject| !subject.trim().is_empty())
            .unwrap_or_else(|| NO_SUBJECT.to_owned());
        let messages = detail
            .messages
            .into_iter()
            .enumerate()
            .map(|(index, detail)| {
                let message = detail.message;
                let headers = &message.headers;
                let unread = message.is_unread();
                MessageItem {
                    sender: headers.from.first().map_or_else(
                        || "(unknown sender)".to_owned(),
                        |from| short_name(from, me),
                    ),
                    from: format_addresses(&headers.from),
                    to: format_addresses(&headers.to),
                    cc: format_addresses(&headers.cc),
                    recipients: recipients_summary(&headers.to, &headers.cc, me),
                    date: message.internal_date,
                    snippet: openmail_mime::html_to_text(&message.snippet)
                        .trim()
                        .to_owned(),
                    unread,
                    expanded: unread || index == last,
                    body: detail.body.as_ref().map_or(BodyState::NotLoaded, |body| {
                        BodyState::Loaded(body_text(body))
                    }),
                    id: message.id,
                }
            })
            .collect();
        Self {
            labels: label_chips(&detail.thread.label_ids, labels),
            thread_id: detail.thread.id,
            subject,
            messages,
        }
    }

    /// Replaces the conversation with a newer copy of the same thread,
    /// keeping what the user expanded or collapsed and the bodies already
    /// loaded or being loaded. Messages new to the thread get the defaults.
    pub fn refresh(&mut self, detail: ThreadDetail, labels: &[Label], me: Option<&str>) {
        let mut fresh = Self::new(detail, labels, me);
        let mut old: HashMap<MessageId, MessageItem> = self
            .messages
            .drain(..)
            .map(|message| (message.id.clone(), message))
            .collect();
        for message in &mut fresh.messages {
            let Some(previous) = old.remove(&message.id) else {
                continue;
            };
            message.expanded = previous.expanded;
            if message.body == BodyState::NotLoaded {
                message.body = previous.body;
            }
        }
        *self = fresh;
    }

    /// Expands a collapsed message or collapses an expanded one. Returns
    /// whether it is now expanded.
    pub fn toggle(&mut self, id: &MessageId) -> bool {
        let Some(message) = self.message_mut(id) else {
            return false;
        };
        message.expanded = !message.expanded;
        message.expanded
    }

    /// The expanded messages whose bodies have not been requested, now
    /// marked as loading. The caller requests each of them.
    pub fn take_bodies_to_load(&mut self) -> Vec<MessageId> {
        self.messages
            .iter_mut()
            .filter(|message| message.expanded && message.body == BodyState::NotLoaded)
            .map(|message| {
                message.body = BodyState::Loading;
                message.id.clone()
            })
            .collect()
    }

    /// Records the answer to a body request: the body, or `None` if the
    /// message no longer exists.
    pub fn body_loaded(&mut self, id: &MessageId, body: Option<&MessageBody>) {
        if let Some(message) = self.message_mut(id) {
            message.body = body.map_or_else(
                || BodyState::Unavailable("This message is no longer available.".to_owned()),
                |body| BodyState::Loaded(body_text(body)),
            );
        }
    }

    /// Records that a body request failed.
    pub fn body_failed(&mut self, id: &MessageId, error: &str) {
        if let Some(message) = self.message_mut(id) {
            message.body =
                BodyState::Unavailable(format!("This message couldn't be loaded: {error}"));
        }
    }

    /// Handles a body the mail service fetched and stored: an expanded
    /// message that has no body yet (its request failed earlier) asks again.
    /// Returns whether a body needs loading.
    pub fn body_fetched(&mut self, id: &MessageId) -> bool {
        match self.message_mut(id) {
            Some(message)
                if message.expanded && matches!(message.body, BodyState::Unavailable(_)) =>
            {
                message.body = BodyState::NotLoaded;
                true
            }
            _ => false,
        }
    }

    fn message_mut(&mut self, id: &MessageId) -> Option<&mut MessageItem> {
        self.messages.iter_mut().find(|message| &message.id == id)
    }
}

/// The text to show for a body: its plain text, or text converted from its
/// HTML when the plain text is missing or blank (the HTML view is Phase 4).
pub fn body_text(body: &MessageBody) -> String {
    let text = body
        .text
        .as_deref()
        .filter(|text| !text.trim().is_empty())
        .map(str::to_owned)
        .or_else(|| body.html.as_deref().map(openmail_mime::html_to_text))
        .unwrap_or_default();
    let text = text.trim_end();
    if text.trim().is_empty() {
        EMPTY_BODY.to_owned()
    } else {
        text.trim_start_matches(['\r', '\n']).to_owned()
    }
}

/// The chips for a thread's labels: Inbox, Spam and Trash first, then user
/// labels by name. Other system labels (unread, starred, categories, …) and
/// labels missing from `labels` are left out.
pub fn label_chips(label_ids: &[LabelId], labels: &[Label]) -> Vec<LabelChip> {
    const SHOWN: [SystemLabel; 3] = [SystemLabel::Inbox, SystemLabel::Spam, SystemLabel::Trash];

    let mut system: Vec<(usize, LabelChip)> = Vec::new();
    let mut user: Vec<LabelChip> = Vec::new();
    for label in labels.iter().filter(|label| label_ids.contains(&label.id)) {
        if label.is_user() {
            user.push(LabelChip {
                id: label.id.clone(),
                name: label.name.clone(),
                color: label.color.clone(),
            });
        } else if let Some(rank) = label
            .system_label()
            .and_then(|system| SHOWN.iter().position(|&shown| shown == system))
        {
            system.push((
                rank,
                LabelChip {
                    id: label.id.clone(),
                    name: SHOWN[rank].display_name().to_owned(),
                    color: None,
                },
            ));
        }
    }
    system.sort_by_key(|(rank, _)| *rank);
    user.sort_by_cached_key(|chip| chip.name.to_lowercase());
    system
        .into_iter()
        .map(|(_, chip)| chip)
        .chain(user)
        .collect()
}

/// Whether `address` is the account's own.
fn is_me(address: &Address, me: Option<&str>) -> bool {
    me.is_some_and(|me| address.email.eq_ignore_ascii_case(me))
}

/// An address as short as possible: "me" for the account's own, else the
/// display name, else the address.
pub fn short_name(address: &Address, me: Option<&str>) -> String {
    if is_me(address, me) {
        "me".to_owned()
    } else {
        display_name(address).to_owned()
    }
}

/// The display name, or the address when there is none.
pub fn display_name(address: &Address) -> &str {
    address
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(&address.email)
}

/// An address in full: `Ada Lovelace <ada@example.com>`, or the bare address
/// when it has no display name.
pub fn format_address(address: &Address) -> String {
    match address.name.as_deref().map(str::trim) {
        Some(name) if !name.is_empty() && name != address.email => {
            format!("{name} <{}>", address.email)
        }
        _ => address.email.clone(),
    }
}

/// Addresses in full, separated by commas.
pub fn format_addresses(addresses: &[Address]) -> String {
    addresses
        .iter()
        .map(format_address)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The recipients (`To`, then `Cc`) in short, as Gmail sums them up under the
/// sender: "to me, Bob Smith". Without any, "to undisclosed recipients".
pub fn recipients_summary(to: &[Address], cc: &[Address], me: Option<&str>) -> String {
    let mut names: Vec<String> = Vec::new();
    for address in to.iter().chain(cc) {
        let name = short_name(address, me);
        if !names.contains(&name) {
            names.push(name);
        }
    }
    if names.is_empty() {
        "to undisclosed recipients".to_owned()
    } else {
        format!("to {}", names.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use openmail_core::{
        HistoryId, LabelListVisibility, LabelType, Message, MessageHeaders, MessageListVisibility,
        Thread,
    };
    use openmail_store::MessageDetail;
    use time::macros::datetime;

    use super::*;

    const ME: Option<&str> = Some("me@example.com");

    fn message(id: &str, from: Address, unread: bool) -> MessageDetail {
        let mut label_ids = vec![SystemLabel::Inbox.id()];
        if unread {
            label_ids.push(SystemLabel::Unread.id());
        }
        MessageDetail {
            message: Message {
                id: MessageId::new(id),
                thread_id: ThreadId::new("t1"),
                history_id: HistoryId::new(1),
                label_ids,
                snippet: format!("Snippet of {id} &amp; more"),
                internal_date: datetime!(2026-11-10 10:00 UTC),
                size_estimate: 100,
                headers: MessageHeaders {
                    from: vec![from],
                    to: vec![Address::new("me@example.com")],
                    subject: Some("Plans".to_owned()),
                    ..MessageHeaders::default()
                },
                has_attachments: false,
            },
            body: None,
        }
    }

    fn detail(messages: Vec<MessageDetail>) -> ThreadDetail {
        ThreadDetail {
            thread: Thread {
                id: ThreadId::new("t1"),
                history_id: HistoryId::new(1),
                subject: Some("Plans".to_owned()),
                snippet: String::new(),
                last_message_date: datetime!(2026-11-10 10:00 UTC),
                message_count: u32::try_from(messages.len()).unwrap(),
                label_ids: vec![SystemLabel::Inbox.id()],
                senders: Vec::new(),
                has_attachments: false,
            },
            messages,
        }
    }

    fn ada() -> Address {
        Address::named("Ada Lovelace", "ada@example.com")
    }

    fn ids(n: usize) -> Vec<MessageDetail> {
        (1..=n)
            .map(|i| message(&format!("m{i}"), ada(), false))
            .collect()
    }

    fn expanded(conversation: &Conversation) -> Vec<bool> {
        conversation.messages.iter().map(|m| m.expanded).collect()
    }

    fn body(text: Option<&str>, html: Option<&str>) -> MessageBody {
        MessageBody {
            text: text.map(str::to_owned),
            html: html.map(str::to_owned),
            fetched_at: datetime!(2026-11-10 10:00 UTC),
        }
    }

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
    fn only_the_latest_read_message_starts_expanded() {
        let conversation = Conversation::new(detail(ids(3)), &[], ME);
        assert_eq!(expanded(&conversation), [false, false, true]);
    }

    #[test]
    fn unread_messages_start_expanded() {
        let mut messages = ids(4);
        messages[1] = message("m2", ada(), true);
        let conversation = Conversation::new(detail(messages), &[], ME);
        assert_eq!(expanded(&conversation), [false, true, false, true]);
    }

    #[test]
    fn a_single_message_is_expanded() {
        let conversation = Conversation::new(detail(ids(1)), &[], ME);
        assert_eq!(expanded(&conversation), [true]);
    }

    #[test]
    fn toggling_expands_and_collapses() {
        let mut conversation = Conversation::new(detail(ids(2)), &[], ME);
        let first = MessageId::new("m1");
        assert!(conversation.toggle(&first));
        assert_eq!(expanded(&conversation), [true, true]);
        assert!(!conversation.toggle(&first));
        assert_eq!(expanded(&conversation), [false, true]);
        assert!(!conversation.toggle(&MessageId::new("missing")));
    }

    #[test]
    fn only_expanded_messages_load_bodies_once() {
        let mut conversation = Conversation::new(detail(ids(3)), &[], ME);
        assert_eq!(conversation.take_bodies_to_load(), [MessageId::new("m3")]);
        assert_eq!(conversation.messages[2].body, BodyState::Loading);
        assert!(conversation.take_bodies_to_load().is_empty());

        conversation.toggle(&MessageId::new("m1"));
        assert_eq!(conversation.take_bodies_to_load(), [MessageId::new("m1")]);
    }

    #[test]
    fn stored_bodies_are_shown_without_loading() {
        let mut messages = ids(1);
        messages[0].body = Some(body(Some("Hello\n"), None));
        let mut conversation = Conversation::new(detail(messages), &[], ME);
        assert_eq!(
            conversation.messages[0].body,
            BodyState::Loaded("Hello".to_owned())
        );
        assert!(conversation.take_bodies_to_load().is_empty());
    }

    #[test]
    fn loaded_and_missing_bodies() {
        let mut conversation = Conversation::new(detail(ids(2)), &[], ME);
        conversation.take_bodies_to_load();
        conversation.body_loaded(&MessageId::new("m2"), Some(&body(Some("Hi"), None)));
        assert_eq!(
            conversation.messages[1].body,
            BodyState::Loaded("Hi".to_owned())
        );
        conversation.body_loaded(&MessageId::new("m2"), None);
        assert!(matches!(
            conversation.messages[1].body,
            BodyState::Unavailable(_)
        ));
    }

    #[test]
    fn a_failed_body_retries_once_fetched() {
        let mut conversation = Conversation::new(detail(ids(2)), &[], ME);
        let latest = MessageId::new("m2");
        conversation.take_bodies_to_load();
        conversation.body_failed(&latest, "offline");
        let BodyState::Unavailable(reason) = &conversation.messages[1].body else {
            panic!("expected unavailable");
        };
        assert!(reason.contains("offline"));

        // A collapsed message, or one already loading, doesn't ask again.
        assert!(!conversation.body_fetched(&MessageId::new("m1")));
        assert!(conversation.body_fetched(&latest));
        assert_eq!(conversation.take_bodies_to_load(), [latest]);
    }

    #[test]
    fn refresh_keeps_toggles_and_bodies_and_expands_new_messages() {
        let mut conversation = Conversation::new(detail(ids(2)), &[], ME);
        conversation.toggle(&MessageId::new("m1"));
        conversation.toggle(&MessageId::new("m2"));
        conversation.take_bodies_to_load();
        conversation.body_loaded(&MessageId::new("m1"), Some(&body(Some("One"), None)));

        conversation.refresh(detail(ids(3)), &[], ME);
        assert_eq!(expanded(&conversation), [true, false, true]);
        assert_eq!(
            conversation.messages[0].body,
            BodyState::Loaded("One".to_owned())
        );
        assert_eq!(conversation.messages[2].body, BodyState::NotLoaded);
    }

    #[test]
    fn subject_falls_back_to_the_first_message_then_a_placeholder() {
        let mut thread = detail(ids(1));
        thread.thread.subject = None;
        assert_eq!(Conversation::new(thread, &[], ME).subject, "Plans");

        let mut thread = detail(ids(1));
        thread.thread.subject = Some("  ".to_owned());
        thread.messages[0].message.headers.subject = None;
        assert_eq!(Conversation::new(thread, &[], ME).subject, NO_SUBJECT);
    }

    #[test]
    fn messages_show_short_senders_and_decoded_snippets() {
        let messages = vec![
            message("m1", ada(), false),
            message("m2", Address::named("Me Myself", "ME@example.com"), false),
        ];
        let conversation = Conversation::new(detail(messages), &[], ME);
        assert_eq!(conversation.messages[0].sender, "Ada Lovelace");
        assert_eq!(
            conversation.messages[0].from,
            "Ada Lovelace <ada@example.com>"
        );
        assert_eq!(conversation.messages[0].recipients, "to me");
        assert_eq!(conversation.messages[0].snippet, "Snippet of m1 & more");
        assert_eq!(conversation.messages[1].sender, "me");
    }

    #[test]
    fn participants_are_formatted_in_full_and_in_short() {
        let to = [
            Address::new("me@example.com"),
            Address::named("Bob Smith", "bob@example.com"),
        ];
        let cc = [
            Address::named("", "carol@example.com"),
            Address::named("Bob Smith", "bob@example.com"),
        ];
        assert_eq!(
            format_addresses(&to),
            "me@example.com, Bob Smith <bob@example.com>"
        );
        assert_eq!(format_addresses(&cc[..1]), "carol@example.com");
        assert_eq!(
            format_address(&Address::named("x@example.com", "x@example.com")),
            "x@example.com"
        );
        assert_eq!(
            recipients_summary(&to, &cc, ME),
            "to me, Bob Smith, carol@example.com"
        );
        assert_eq!(
            recipients_summary(&to, &[], None),
            "to me@example.com, Bob Smith"
        );
        assert_eq!(
            recipients_summary(&[], &[], ME),
            "to undisclosed recipients"
        );
    }

    #[test]
    fn bodies_fall_back_to_text_from_html() {
        assert_eq!(body_text(&body(Some("\nHi\n\n"), None)), "Hi");
        assert_eq!(
            body_text(&body(Some(" \r\n"), Some("<p>From &amp; HTML</p>"))),
            "From & HTML"
        );
        assert_eq!(body_text(&body(None, Some("<b>Bold</b>"))), "Bold");
        assert_eq!(body_text(&body(None, None)), EMPTY_BODY);
    }

    #[test]
    fn chips_show_inbox_spam_trash_then_user_labels_by_name() {
        let labels = [
            label("Label_2", "work/projects", LabelType::User),
            label("TRASH", "TRASH", LabelType::System),
            label("UNREAD", "UNREAD", LabelType::System),
            label("CATEGORY_SOCIAL", "CATEGORY_SOCIAL", LabelType::System),
            label("Label_1", "Receipts", LabelType::User),
            label("INBOX", "INBOX", LabelType::System),
            label("Label_3", "Unused", LabelType::User),
        ];
        let on_thread = [
            "UNREAD",
            "Label_2",
            "CATEGORY_SOCIAL",
            "INBOX",
            "TRASH",
            "Label_1",
            "Label_9",
        ]
        .map(LabelId::new);
        let names: Vec<String> = label_chips(&on_thread, &labels)
            .into_iter()
            .map(|chip| chip.name)
            .collect();
        assert_eq!(names, ["Inbox", "Trash", "Receipts", "work/projects"]);
    }
}
