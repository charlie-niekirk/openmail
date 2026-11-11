//! The conversation view-model: an open thread's header and its messages,
//! collapsed or expanded the way Gmail shows them, with every message's body
//! loaded when the thread opens.
//!
//! [`ReadingPane`] is what the pane shows: nothing, a thread loading or
//! failing to, or the open [`Conversation`]. Neither has GPUI types;
//! `conversation_view` renders them and drives their loading.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use openmail_core::{Address, Label, LabelId, MessageId, SystemLabel, ThreadId};
use openmail_store::{MessageBody, ThreadDetail};
use time::OffsetDateTime;

use crate::sanitize::{CleanHtml, CleanOptions, clean_html};
pub use crate::thread_list::LabelChip;

/// What a thread with no subject is called.
pub const NO_SUBJECT: &str = "(no subject)";

/// What an expanded message shows when its body has no text at all.
pub const EMPTY_BODY: &str = "(This message has no text.)";

/// What an expanded message says when its body can't be fetched offline.
pub const OFFLINE_BODY: &str = "You're offline. This message will load once you reconnect.";

/// What the reading pane shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ReadingPane {
    /// No thread is open.
    #[default]
    Empty,
    /// The thread is being read from the store.
    Loading(ThreadId),
    /// The thread is not in the store (deleted, or not synced).
    Missing(ThreadId),
    /// Reading the thread failed; a retry is offered.
    Failed(ThreadId, String),
    Open(Conversation),
}

impl ReadingPane {
    /// The thread shown, or being loaded, if any.
    pub fn thread_id(&self) -> Option<&ThreadId> {
        match self {
            Self::Empty => None,
            Self::Loading(id) | Self::Missing(id) | Self::Failed(id, _) => Some(id),
            Self::Open(conversation) => Some(&conversation.thread_id),
        }
    }

    /// The open conversation, if it has loaded.
    pub fn conversation_mut(&mut self) -> Option<&mut Conversation> {
        match self {
            Self::Open(conversation) => Some(conversation),
            _ => None,
        }
    }

    /// Starts loading `id`, replacing what is shown. Returns `false` if it
    /// is already shown.
    pub fn open(&mut self, id: ThreadId) -> bool {
        if self.thread_id() == Some(&id) {
            return false;
        }
        *self = Self::Loading(id);
        true
    }

    /// Shows nothing. Returns whether a thread was shown.
    pub fn close(&mut self) -> bool {
        std::mem::take(self) != Self::Empty
    }

    /// Loads a thread that failed again. Returns whether one had.
    pub fn retry(&mut self) -> bool {
        match self {
            Self::Failed(id, _) => {
                *self = Self::Loading(id.clone());
                true
            }
            _ => false,
        }
    }

    /// Applies a load of thread `id` (with the account's `labels` and
    /// address `me`). An open thread refreshes in place, keeping what the
    /// user expanded, and keeps showing if a refresh fails. Returns `false`,
    /// changing nothing, if `id` is no longer the thread shown.
    pub fn loaded(
        &mut self,
        id: &ThreadId,
        result: Result<Option<ThreadDetail>, String>,
        labels: &[Label],
        me: Option<&str>,
    ) -> bool {
        if self.thread_id() != Some(id) {
            return false;
        }
        match (result, &mut *self) {
            (Ok(Some(detail)), Self::Open(conversation)) => {
                conversation.refresh(detail, labels, me);
            }
            (Ok(Some(detail)), _) => *self = Self::Open(Conversation::new(detail, labels, me)),
            (Ok(None), _) => *self = Self::Missing(id.clone()),
            (Err(_), Self::Open(_)) => {}
            (Err(error), _) => *self = Self::Failed(id.clone(), error),
        }
        true
    }
}

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

/// One message in a [`Conversation`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageItem {
    pub id: MessageId,
    /// Who sent it, as short as possible: the display name, or "me".
    pub sender: String,
    /// The sender's address, lowercased, for "always show images from".
    pub sender_address: Option<String>,
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
    /// Whether its remote images load (the user chose to show them, for
    /// this message or its sender).
    pub show_images: bool,
    pub body: BodyState,
}

impl MessageItem {
    /// Whether its body has remote images that are hidden.
    pub fn images_hidden(&self) -> bool {
        !self.show_images
            && matches!(&self.body, BodyState::Loaded(LoadedBody { html: Some(html), .. })
                if html.blocked_images)
    }

    /// Shows its remote images, cleaning its body again to let them in.
    /// Returns whether anything changed.
    fn show_images(&mut self) -> bool {
        if self.show_images {
            return false;
        }
        self.show_images = true;
        if let BodyState::Loaded(body) = &mut self.body {
            body.clean(&self.id, true);
        }
        true
    }
}

/// Where an expanded message's body is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyState {
    /// Not requested yet (collapsed messages don't ask).
    NotLoaded,
    /// Requested from the mail service.
    Loading,
    /// The body, ready to show.
    Loaded(LoadedBody),
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
        Self::build(detail, labels, me, HashMap::new())
    }

    /// [`Conversation::new`], carrying over from `previous` what the user
    /// expanded or collapsed and the bodies already loaded or being loaded.
    fn build(
        detail: ThreadDetail,
        labels: &[Label],
        me: Option<&str>,
        mut previous: HashMap<MessageId, MessageItem>,
    ) -> Self {
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
                let before = previous.remove(&message.id);
                let show_images = before.as_ref().is_some_and(|before| before.show_images);
                // A body loaded before is kept rather than cleaned again.
                let body = match before.as_ref().map(|before| &before.body) {
                    Some(body @ BodyState::Loaded(_)) => body.clone(),
                    before_body => detail.body.as_ref().map_or_else(
                        || before_body.cloned().unwrap_or(BodyState::NotLoaded),
                        |body| BodyState::Loaded(LoadedBody::new(&message.id, body, show_images)),
                    ),
                };
                MessageItem {
                    sender: headers.from.first().map_or_else(
                        || "(unknown sender)".to_owned(),
                        |from| short_name(from, me),
                    ),
                    sender_address: headers
                        .from
                        .first()
                        .map(|from| from.email.trim().to_lowercase()),
                    from: format_addresses(&headers.from),
                    to: format_addresses(&headers.to),
                    cc: format_addresses(&headers.cc),
                    recipients: recipients_summary(&headers.to, &headers.cc, me),
                    date: message.internal_date,
                    snippet: openmail_core::decode_entities(&message.snippet)
                        .trim()
                        .to_owned(),
                    unread,
                    expanded: before.map_or(unread || index == last, |before| before.expanded),
                    show_images,
                    body,
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
        let previous = self
            .messages
            .drain(..)
            .map(|message| (message.id.clone(), message))
            .collect();
        *self = Self::build(detail, labels, me, previous);
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

    /// The messages whose bodies have not been requested, now marked as
    /// loading. The caller requests each of them (the mail service fetches
    /// requests that arrive together in one batch).
    ///
    /// Every message's body loads, collapsed or not: the webview opens and
    /// closes messages itself (`<details>`), out of the view-model's sight.
    pub fn take_bodies_to_load(&mut self) -> Vec<MessageId> {
        self.messages
            .iter_mut()
            .filter(|message| message.body == BodyState::NotLoaded)
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
            let show_images = message.show_images;
            message.body = body.map_or_else(
                || BodyState::Unavailable("This message is no longer available.".to_owned()),
                |body| BodyState::Loaded(LoadedBody::new(id, body, show_images)),
            );
        }
    }

    /// Shows the remote images of message `id`. Returns whether anything
    /// changed.
    pub fn show_images(&mut self, id: &MessageId) -> bool {
        self.message_mut(id).is_some_and(MessageItem::show_images)
    }

    /// Shows the remote images of every message from one of `senders`
    /// (lowercased addresses): those whose images are always shown. Returns
    /// whether anything changed.
    pub fn show_images_from(&mut self, senders: &BTreeSet<String>) -> bool {
        let mut changed = false;
        for message in &mut self.messages {
            if message
                .sender_address
                .as_ref()
                .is_some_and(|address| senders.contains(address))
            {
                changed |= message.show_images();
            }
        }
        changed
    }

    /// The messages whose remote images are hidden, oldest first.
    pub fn hidden_images(&self) -> impl Iterator<Item = &MessageItem> {
        self.messages
            .iter()
            .filter(|message| message.images_hidden())
    }

    /// Records that a body request failed.
    pub fn body_failed(&mut self, id: &MessageId, error: &str) {
        if let Some(message) = self.message_mut(id) {
            message.body =
                BodyState::Unavailable(format!("This message couldn't be loaded: {error}"));
        }
    }

    /// The body of `id` couldn't be fetched because Gmail is unreachable.
    pub fn body_offline(&mut self, id: &MessageId) {
        if let Some(message) = self.message_mut(id) {
            message.body = BodyState::Unavailable(OFFLINE_BODY.to_owned());
        }
    }

    /// Whether every body has loaded or failed to, so the conversation can
    /// be shown whole.
    pub fn bodies_settled(&self) -> bool {
        self.messages.iter().all(|message| {
            matches!(
                message.body,
                BodyState::Loaded(_) | BodyState::Unavailable(_)
            )
        })
    }

    /// Asks again for the bodies that were unavailable, as after
    /// reconnecting. Returns whether any need loading.
    pub fn retry_unavailable(&mut self) -> bool {
        let mut any = false;
        for message in &mut self.messages {
            if matches!(message.body, BodyState::Unavailable(_)) {
                message.body = BodyState::NotLoaded;
                any = true;
            }
        }
        any
    }

    /// Handles a body the mail service fetched and stored: a message that
    /// has no body yet (its request failed earlier) asks again. Returns
    /// whether a body needs loading.
    pub fn body_fetched(&mut self, id: &MessageId) -> bool {
        match self.message_mut(id) {
            Some(message) if matches!(message.body, BodyState::Unavailable(_)) => {
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

/// A body as the reading pane shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedBody {
    /// Its text ([`body_text`]), shown when it has no HTML or when the HTML
    /// view is unavailable.
    pub text: String,
    /// Its HTML part, sanitized ([`clean_html`]), if it has one with content.
    pub html: Option<CleanHtml>,
    /// The body as stored, to clean again once its images are shown.
    pub source: Option<Arc<MessageBody>>,
}

impl LoadedBody {
    /// `body`, the body of message `id`, ready to show, with its remote
    /// images if `remote_images`.
    pub fn new(id: &MessageId, body: &MessageBody, remote_images: bool) -> Self {
        let mut loaded = Self {
            text: body_text(body),
            html: None,
            source: Some(Arc::new(body.clone())),
        };
        loaded.clean(id, remote_images);
        loaded
    }

    /// Cleans its HTML part (again), with remote images or without.
    fn clean(&mut self, id: &MessageId, remote_images: bool) {
        let Some(source) = &self.source else {
            return;
        };
        let options = CleanOptions {
            message_id: id.as_str(),
            remote_images,
            inline: &source.inline,
        };
        self.html = source
            .html
            .as_deref()
            .filter(|html| !html.trim().is_empty())
            .map(|html| clean_html(html, &options))
            .filter(|cleaned| !cleaned.html.trim().is_empty());
    }
}

/// The text to show for a body: its plain text, or text converted from its
/// HTML when the plain text is missing or blank.
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
            inline: Vec::new(),
        }
    }

    fn loaded_body(body: &MessageBody) -> LoadedBody {
        LoadedBody::new(&MessageId::new("m1"), body, false)
    }

    fn loaded_text(message: &MessageItem) -> Option<&str> {
        match &message.body {
            BodyState::Loaded(body) => Some(&body.text),
            _ => None,
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
    fn every_body_loads_once() {
        let mut conversation = Conversation::new(detail(ids(3)), &[], ME);
        assert_eq!(
            conversation.take_bodies_to_load(),
            ["m1", "m2", "m3"].map(MessageId::new)
        );
        assert_eq!(conversation.messages[0].body, BodyState::Loading);
        assert!(conversation.take_bodies_to_load().is_empty());

        conversation.toggle(&MessageId::new("m1"));
        assert!(conversation.take_bodies_to_load().is_empty());
    }

    #[test]
    fn bodies_settle_once_each_has_loaded_or_failed() {
        let mut conversation = Conversation::new(detail(ids(2)), &[], ME);
        assert!(!conversation.bodies_settled());
        conversation.take_bodies_to_load();
        assert!(!conversation.bodies_settled());
        conversation.body_loaded(&MessageId::new("m1"), Some(&body(Some("Hi"), None)));
        assert!(!conversation.bodies_settled());
        conversation.body_offline(&MessageId::new("m2"));
        assert!(conversation.bodies_settled());
    }

    #[test]
    fn stored_bodies_are_shown_without_loading() {
        let mut messages = ids(1);
        messages[0].body = Some(body(Some("Hello\n"), None));
        let mut conversation = Conversation::new(detail(messages), &[], ME);
        assert_eq!(loaded_text(&conversation.messages[0]), Some("Hello"));
        assert!(conversation.take_bodies_to_load().is_empty());
    }

    #[test]
    fn loaded_and_missing_bodies() {
        let mut conversation = Conversation::new(detail(ids(2)), &[], ME);
        conversation.take_bodies_to_load();
        conversation.body_loaded(&MessageId::new("m2"), Some(&body(Some("Hi"), None)));
        assert_eq!(loaded_text(&conversation.messages[1]), Some("Hi"));
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

        // A message whose body is loading doesn't ask again.
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
        assert_eq!(loaded_text(&conversation.messages[0]), Some("One"));
        assert_eq!(conversation.messages[2].body, BodyState::NotLoaded);
    }

    #[test]
    fn refresh_keeps_loaded_bodies_and_takes_newly_stored_ones() {
        let mut conversation = Conversation::new(detail(ids(2)), &[], ME);
        conversation.take_bodies_to_load();
        conversation.body_loaded(&MessageId::new("m1"), Some(&body(Some("One"), None)));
        conversation.body_offline(&MessageId::new("m2"));

        let mut messages = ids(2);
        messages[0].body = Some(body(Some("Stored"), None));
        messages[1].body = Some(body(Some("Two"), None));
        conversation.refresh(detail(messages), &[], ME);
        assert_eq!(loaded_text(&conversation.messages[0]), Some("One"));
        assert_eq!(loaded_text(&conversation.messages[1]), Some("Two"));
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
    fn html_parts_are_cleaned_and_blank_ones_dropped() {
        let loaded = loaded_body(&body(
            Some("Hi"),
            Some("<p onclick=\"x()\">Hi</p><script>x()</script>"),
        ));
        assert_eq!(loaded.text, "Hi");
        assert_eq!(
            loaded.html.map(|cleaned| cleaned.html).as_deref(),
            Some("<p>Hi</p>")
        );

        assert_eq!(loaded_body(&body(Some("Hi"), Some(" \n"))).html, None);
        assert_eq!(
            loaded_body(&body(Some("Hi"), Some("<script>x()</script>"))).html,
            None
        );
        let text_only = loaded_body(&body(None, Some("<b>Bold</b>")));
        assert_eq!(text_only.text, "Bold");
        assert_eq!(
            text_only.html.map(|cleaned| cleaned.html).as_deref(),
            Some("<b>Bold</b>")
        );
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

    #[test]
    fn the_reading_pane_opens_and_closes_threads() {
        let mut pane = ReadingPane::default();
        let t1 = ThreadId::new("t1");
        assert!(!pane.close(), "nothing to close");

        assert!(pane.open(t1.clone()));
        assert_eq!(pane, ReadingPane::Loading(t1.clone()));
        assert!(!pane.open(t1.clone()), "already opening");

        assert!(pane.loaded(&t1, Ok(Some(detail(ids(2)))), &[], ME));
        assert_eq!(pane.thread_id(), Some(&t1));
        assert!(pane.conversation_mut().is_some());
        assert!(!pane.open(t1.clone()), "already open");

        assert!(pane.close());
        assert_eq!(pane, ReadingPane::Empty);
        assert_eq!(pane.thread_id(), None);
    }

    #[test]
    fn late_loads_for_another_thread_are_dropped() {
        let mut pane = ReadingPane::default();
        pane.open(ThreadId::new("t1"));
        pane.open(ThreadId::new("t2"));
        assert!(!pane.loaded(&ThreadId::new("t1"), Ok(Some(detail(ids(1)))), &[], ME));
        assert_eq!(pane, ReadingPane::Loading(ThreadId::new("t2")));

        pane.close();
        assert!(!pane.loaded(&ThreadId::new("t2"), Ok(None), &[], ME));
        assert_eq!(pane, ReadingPane::Empty);
    }

    #[test]
    fn missing_and_failed_threads_and_retry() {
        let t1 = ThreadId::new("t1");
        let mut pane = ReadingPane::default();
        pane.open(t1.clone());
        pane.loaded(&t1, Ok(None), &[], ME);
        assert_eq!(pane, ReadingPane::Missing(t1.clone()));
        assert!(!pane.retry(), "only failures retry");

        let mut pane = ReadingPane::default();
        pane.open(t1.clone());
        pane.loaded(&t1, Err("disk I/O error".to_owned()), &[], ME);
        assert_eq!(
            pane,
            ReadingPane::Failed(t1.clone(), "disk I/O error".to_owned())
        );
        assert!(pane.retry());
        assert_eq!(pane, ReadingPane::Loading(t1.clone()));
        pane.loaded(&t1, Ok(Some(detail(ids(1)))), &[], ME);
        assert!(matches!(pane, ReadingPane::Open(_)));
    }

    #[test]
    fn an_open_thread_survives_a_failed_refresh() {
        let t1 = ThreadId::new("t1");
        let mut pane = ReadingPane::default();
        pane.open(t1.clone());
        pane.loaded(&t1, Ok(Some(detail(ids(2)))), &[], ME);
        let before = pane.clone();
        assert!(pane.loaded(&t1, Err("busy".to_owned()), &[], ME));
        assert_eq!(pane, before);
    }

    #[test]
    fn offline_bodies_retry_on_reconnect() {
        let mut conversation = Conversation::new(detail(ids(2)), &[], ME);
        let latest = MessageId::new("m2");
        assert_eq!(conversation.take_bodies_to_load().len(), 2);
        conversation.body_loaded(&MessageId::new("m1"), Some(&body(Some("Hi"), None)));
        conversation.body_offline(&latest);
        assert_eq!(
            conversation.messages[1].body,
            BodyState::Unavailable(OFFLINE_BODY.to_owned())
        );
        // Only the body that couldn't be fetched is asked for again.
        assert!(conversation.retry_unavailable());
        assert_eq!(conversation.take_bodies_to_load(), [latest]);
        assert!(!conversation.retry_unavailable(), "nothing left to retry");
    }

    fn html(message: &MessageItem) -> &CleanHtml {
        match &message.body {
            BodyState::Loaded(LoadedBody {
                html: Some(html), ..
            }) => html,
            body => panic!("no HTML: {body:?}"),
        }
    }

    const PIXEL: &str = "<p>Hi</p><img src=\"https://t.example/open.gif\">";

    #[test]
    fn remote_images_stay_hidden_until_shown() {
        let mut conversation = Conversation::new(detail(ids(2)), &[], ME);
        let (m1, m2) = (MessageId::new("m1"), MessageId::new("m2"));
        conversation.take_bodies_to_load();
        conversation.body_loaded(&m1, Some(&body(Some("Hi"), Some(PIXEL))));
        conversation.body_loaded(&m2, Some(&body(Some("Hi"), Some("<p>Hi</p>"))));
        let hidden: Vec<&MessageId> = conversation.hidden_images().map(|m| &m.id).collect();
        assert_eq!(hidden, [&m1]);
        assert!(!html(&conversation.messages[0]).html.contains("t.example"));

        assert!(conversation.show_images(&m1));
        assert!(!conversation.show_images(&m1), "already shown");
        assert_eq!(conversation.hidden_images().count(), 0);
        let shown = html(&conversation.messages[0]);
        assert!(shown.html.contains("src=\"https://t.example/open.gif\""));
        assert_eq!(
            shown.image_urls.iter().collect::<Vec<_>>(),
            ["https://t.example/open.gif"]
        );

        // A refresh keeps the choice.
        conversation.refresh(detail(ids(2)), &[], ME);
        assert!(conversation.messages[0].show_images);
        assert_eq!(conversation.hidden_images().count(), 0);
    }

    #[test]
    fn senders_whose_images_are_always_shown() {
        let mut messages = ids(2);
        messages[1].message.headers.from = vec![Address::new("News@Example.com")];
        let mut conversation = Conversation::new(detail(messages), &[], ME);
        let senders: BTreeSet<String> = ["news@example.com".to_owned()].into();
        // Before its body loads: the body loads with its images.
        assert!(conversation.show_images_from(&senders));
        assert!(!conversation.show_images_from(&senders));
        conversation.take_bodies_to_load();
        for id in ["m1", "m2"] {
            conversation.body_loaded(&MessageId::new(id), Some(&body(None, Some(PIXEL))));
        }
        let hidden: Vec<&str> = conversation
            .hidden_images()
            .map(|m| m.id.as_str())
            .collect();
        assert_eq!(hidden, ["m1"]);
        assert!(html(&conversation.messages[1]).html.contains("t.example"));
    }
}
