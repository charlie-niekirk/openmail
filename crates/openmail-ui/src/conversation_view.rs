//! The reading pane: the open conversation's header (subject, labels and
//! message count, in GPUI), then its messages, or the pane's state in their
//! place. The state is a [`ReadingPane`]; this view loads it and keeps it
//! current, retrying bodies that failed offline once Gmail is reachable again.
//!
//! The messages are one document in the window's webview
//! ([`MessageBodyView`]), shown once every body has loaded or failed to.
//! Where no webview can be created (a native Wayland window, for example)
//! they are drawn in GPUI as text instead, collapsed to a line or expanded to
//! participants and body.

use std::collections::BTreeSet;

use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, FontWeight, Hsla, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Task, Window, div, px, rgb,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::scroll::ScrollableElement as _;
use gpui_component::spinner::Spinner;
use gpui_component::tag::Tag;
use gpui_component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use openmail_core::{Label, LabelColor, MessageId, ThreadId};
use openmail_store::ThreadDetail;
use time::OffsetDateTime;

use crate::body_document::conversation_document;
use crate::bridge::{AppEvent, AppModel};
use crate::conversation::{BodyState, Conversation, LabelChip, MessageItem, ReadingPane};
use crate::message_body::MessageBodyView;
use crate::placeholder::{banner, loading, placeholder};
use crate::sidebar::parse_hex_color;
use crate::state::Session;
use crate::thread_list::format_date;
use crate::time_format::{full_date, local};
use crate::typography::{TextRole, TypographyExt as _};

/// Widest a conversation's text runs, for readable lines on wide windows.
const MAX_WIDTH: gpui::Pixels = px(860.);

/// The most messages with hidden images listed one by one above the
/// conversation; past it, one line offers to show them all.
const MAX_IMAGE_ROWS: usize = 3;

/// The conversation view. Open a thread with [`ConversationView::open_thread`].
pub struct ConversationView {
    model: Entity<AppModel>,
    pane: ReadingPane,
    /// The account the open thread belongs to.
    account: Option<String>,
    /// Whether Gmail was reachable when last told.
    online: bool,
    /// Loading the thread (replacing it cancels the previous load).
    load: Option<Task<()>>,
    /// The webview showing the conversation.
    body: Entity<MessageBodyView>,
    /// The thread whose document the webview was last given.
    shown: Option<ThreadId>,
    _subscriptions: [Subscription; 2],
}

impl ConversationView {
    pub fn new(cx: &mut Context<'_, Self>) -> Self {
        let model = AppModel::global(cx);
        let events = cx.subscribe(&model, |this, _, event: &AppEvent, cx| match event {
            AppEvent::ThreadsChanged | AppEvent::LabelsChanged => this.reload(cx),
            AppEvent::BodyFetched(id) => this.body_fetched(id, cx),
        });
        let status = cx.observe(&model, |this, _, cx| this.status_changed(cx));
        let mut view = Self {
            model,
            pane: ReadingPane::Empty,
            account: None,
            online: true,
            load: None,
            body: cx.new(MessageBodyView::new),
            shown: None,
            _subscriptions: [events, status],
        };
        view.status_changed(cx);
        view
    }

    /// Opens `id`, replacing the open thread.
    pub fn open_thread(&mut self, id: ThreadId, cx: &mut Context<'_, Self>) {
        if self.pane.open(id) {
            self.reload(cx);
            self.sync_body(cx);
            cx.notify();
        }
    }

    /// Closes the open thread, if any.
    pub fn close(&mut self, cx: &mut Context<'_, Self>) {
        if self.pane.close() {
            self.load = None;
            self.sync_body(cx);
            cx.notify();
        }
    }

    /// The open thread, if any.
    pub fn thread_id(&self) -> Option<&ThreadId> {
        self.pane.thread_id()
    }

    /// Loads a thread that failed again.
    fn retry(&mut self, cx: &mut Context<'_, Self>) {
        if self.pane.retry() {
            self.reload(cx);
            self.sync_body(cx);
            cx.notify();
        }
    }

    /// The signed-in account, if the mailbox is open.
    fn current_account(&self, cx: &App) -> Option<String> {
        match self.model.read(cx).state().session() {
            Session::Ready { account } => Some(account.clone()),
            _ => None,
        }
    }

    /// Follows the session: a different account (or signing out) closes the
    /// open thread and drops the webview, a thread that waited for the mail
    /// service loads once it runs, and bodies that couldn't be fetched load
    /// again once Gmail is reachable.
    fn status_changed(&mut self, cx: &mut Context<'_, Self>) {
        let account = self.current_account(cx);
        if account != self.account {
            self.account = account;
            self.pane = ReadingPane::Empty;
            self.load = None;
            self.shown = None;
            self.body.update(cx, MessageBodyView::tear_down);
            cx.notify();
        }
        if matches!(self.pane, ReadingPane::Loading(_)) && self.load.is_none() {
            self.reload(cx);
        }
        let online = self.model.read(cx).state().is_online();
        let reconnected = online && !self.online;
        self.online = online;
        if reconnected
            && let Some(conversation) = self.pane.conversation_mut()
            && conversation.retry_unavailable()
        {
            self.load_bodies(cx);
            self.sync_body(cx);
            cx.notify();
        }
    }

    /// Queries the open thread (and the labels, for its chips) again.
    fn reload(&mut self, cx: &mut Context<'_, Self>) {
        let Some(id) = self.pane.thread_id().cloned() else {
            return;
        };
        let Some(mail) = self.model.read(cx).mail().cloned() else {
            self.load = None;
            return;
        };
        self.load = Some(cx.spawn(async move |this, cx| {
            let detail = mail.thread(id.clone()).await;
            let labels = mail.labels().await.unwrap_or_default();
            let image_senders: BTreeSet<String> = mail
                .image_senders()
                .await
                .unwrap_or_default()
                .into_iter()
                .collect();
            this.update(cx, |this, cx| {
                this.loaded(&id, detail, &labels, &image_senders, cx);
            })
            .ok();
        }));
    }

    fn loaded(
        &mut self,
        id: &ThreadId,
        detail: openmail_sync::Result<Option<ThreadDetail>>,
        labels: &[Label],
        image_senders: &BTreeSet<String>,
        cx: &mut Context<'_, Self>,
    ) {
        let me = self.account.as_deref();
        let detail = detail.map_err(|err| err.to_string());
        if self.pane.loaded(id, detail, labels, me) {
            if let Some(conversation) = self.pane.conversation_mut() {
                conversation.show_images_from(image_senders);
            }
            self.load_bodies(cx);
            self.sync_body(cx);
            cx.notify();
        }
    }

    /// Gives the webview what the pane now shows: the conversation's
    /// document once every body is in, nothing for an empty or failed pane.
    /// While a thread or its bodies load, the document already up stays: for
    /// the same thread until the new one is ready, for another thread only
    /// briefly ([`MessageBodyView::hold`]), so switching doesn't flash.
    fn sync_body(&mut self, cx: &mut Context<'_, Self>) {
        match &self.pane {
            ReadingPane::Open(conversation) if conversation.bodies_settled() => {
                let now = local(OffsetDateTime::now_utc());
                let document = conversation_document(conversation, |date| {
                    let date = local(date);
                    (format_date(date, now), full_date(date))
                });
                self.shown = Some(conversation.thread_id.clone());
                self.body.update(cx, |body, cx| body.show(document, cx));
            }
            ReadingPane::Open(conversation)
                if self.shown.as_ref() == Some(&conversation.thread_id) => {}
            ReadingPane::Open(_) | ReadingPane::Loading(_) => {
                self.body.update(cx, MessageBodyView::hold);
            }
            ReadingPane::Empty | ReadingPane::Missing(_) | ReadingPane::Failed(..) => {
                self.shown = None;
                self.body.update(cx, MessageBodyView::clear);
            }
        }
    }

    /// Requests the bodies of messages that have none.
    fn load_bodies(&mut self, cx: &mut Context<'_, Self>) {
        let Some(conversation) = self.pane.conversation_mut() else {
            return;
        };
        let Some(mail) = self.model.read(cx).mail().cloned() else {
            return;
        };
        for id in conversation.take_bodies_to_load() {
            let mail = mail.clone();
            // Not cancelled when another thread opens: the body is stored
            // either way, and the answer is dropped below.
            cx.spawn(async move |this, cx| {
                let body = mail.body(id.clone()).await;
                this.update(cx, |this, cx| {
                    let online = this.online;
                    if let Some(conversation) = this.pane.conversation_mut() {
                        match body {
                            Ok(body) => conversation.body_loaded(&id, body.as_ref()),
                            Err(_) if !online => conversation.body_offline(&id),
                            Err(error) => conversation.body_failed(&id, &error.to_string()),
                        }
                        this.sync_body(cx);
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        }
    }

    fn body_fetched(&mut self, id: &MessageId, cx: &mut Context<'_, Self>) {
        if let Some(conversation) = self.pane.conversation_mut()
            && conversation.body_fetched(id)
        {
            self.load_bodies(cx);
            self.sync_body(cx);
            cx.notify();
        }
    }

    /// Shows the remote images of message `id`.
    fn show_images(&mut self, id: &MessageId, cx: &mut Context<'_, Self>) {
        if let Some(conversation) = self.pane.conversation_mut()
            && conversation.show_images(id)
        {
            self.sync_body(cx);
            cx.notify();
        }
    }

    /// Shows the remote images of every message in the conversation.
    fn show_all_images(&mut self, cx: &mut Context<'_, Self>) {
        let Some(conversation) = self.pane.conversation_mut() else {
            return;
        };
        let hidden: Vec<MessageId> = conversation.hidden_images().map(|m| m.id.clone()).collect();
        for id in &hidden {
            conversation.show_images(id);
        }
        if !hidden.is_empty() {
            self.sync_body(cx);
            cx.notify();
        }
    }

    /// Shows remote images from `address` now and from now on, saved for
    /// the account.
    fn always_show_images(&mut self, address: String, cx: &mut Context<'_, Self>) {
        if let Some(conversation) = self.pane.conversation_mut()
            && conversation.show_images_from(&BTreeSet::from([address.clone()]))
        {
            self.sync_body(cx);
            cx.notify();
        }
        if let Some(mail) = self.model.read(cx).mail().cloned() {
            cx.background_spawn(async move {
                if let Err(err) = mail.allow_images_from(address).await {
                    tracing::warn!(%err, "could not save the sender whose images to show");
                }
            })
            .detach();
        }
    }

    /// The strip offering to show the remote images hidden in the open
    /// conversation, if any are.
    fn images_bar(
        &self,
        conversation: &Conversation,
        cx: &mut Context<'_, Self>,
    ) -> Option<AnyElement> {
        let hidden: Vec<&MessageItem> = conversation.hidden_images().collect();
        if hidden.is_empty() {
            return None;
        }
        if hidden.len() > MAX_IMAGE_ROWS {
            let text = format!("Images are hidden in {} messages.", hidden.len());
            return Some(
                banner(text, cx)
                    .child(
                        Button::new("show-all-images")
                            .outline()
                            .small()
                            .label("Show all images")
                            .on_click(cx.listener(|this, _, _, cx| this.show_all_images(cx))),
                    )
                    .into_any_element(),
            );
        }

        let now = local(OffsetDateTime::now_utc());
        let single = conversation.messages.len() == 1;
        let rows = hidden.into_iter().map(|message| {
            let text = if single {
                "Images in this message are hidden to protect your privacy.".to_owned()
            } else {
                format!(
                    "Images in the message from {} ({}) are hidden.",
                    message.sender,
                    format_date(local(message.date), now)
                )
            };
            let id = message.id.clone();
            let show = Button::new(SharedString::from(format!("show-images-{}", message.id)))
                .outline()
                .small()
                .label("Show images")
                .on_click(cx.listener(move |this, _, _, cx| this.show_images(&id, cx)));
            let always = message.sender_address.clone().map(|address| {
                Button::new(SharedString::from(format!("always-images-{}", message.id)))
                    .ghost()
                    .small()
                    .label(format!("Always show from {address}"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.always_show_images(address.clone(), cx);
                    }))
            });
            banner(text, cx).child(show).children(always)
        });
        Some(
            v_flex()
                .flex_none()
                .w_full()
                .children(rows)
                .into_any_element(),
        )
    }

    /// Expands or collapses a message drawn in GPUI (without a webview).
    fn toggle(&mut self, id: &MessageId, cx: &mut Context<'_, Self>) {
        if let Some(conversation) = self.pane.conversation_mut() {
            conversation.toggle(id);
            cx.notify();
        }
    }

    fn render_conversation(
        &self,
        conversation: &Conversation,
        cx: &mut Context<'_, Self>,
    ) -> AnyElement {
        let now = local(OffsetDateTime::now_utc());
        let count = conversation.messages.len();
        let header = v_flex()
            .gap_2()
            .child(
                div()
                    .text_role(TextRole::Title)
                    .child(conversation.subject.clone()),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_2()
                    .children(conversation.labels.iter().map(label_chip))
                    .child(
                        div()
                            .text_role(TextRole::Caption)
                            .text_color(cx.theme().muted_foreground)
                            .child(if count == 1 {
                                "1 message".to_owned()
                            } else {
                                format!("{count} messages")
                            }),
                    ),
            );

        let header = div()
            .flex_none()
            .w_full()
            .max_w(MAX_WIDTH)
            .mx_auto()
            .px_6()
            .pt_5()
            .pb_3()
            .child(header);

        let body = self.body.read(cx);
        let (unavailable, has_document) = (body.unavailable().cloned(), body.has_document());
        let messages = if let Some(reason) = unavailable {
            let messages: Vec<AnyElement> = conversation
                .messages
                .iter()
                .map(|message| self.render_message(message, now, cx))
                .collect();
            v_flex()
                .flex_1()
                .min_h_0()
                .child(banner(format!("{reason} Showing text instead."), cx))
                .child(
                    v_flex()
                        .id(SharedString::from(format!(
                            "conversation-{}",
                            conversation.thread_id
                        )))
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scrollbar()
                        .child(
                            v_flex()
                                .w_full()
                                .max_w(MAX_WIDTH)
                                .mx_auto()
                                .px_6()
                                .pt_1()
                                .pb_5()
                                .gap_3()
                                .children(messages),
                        ),
                )
                .into_any_element()
        } else if conversation.bodies_settled() || has_document {
            // While the next document is prepared, the webview keeps the
            // previous one in place (see `sync_body`).
            v_flex()
                .flex_1()
                .min_h_0()
                .children(self.images_bar(conversation, cx))
                .child(div().flex_1().min_h_0().child(self.body.clone()))
                .into_any_element()
        } else {
            loading().flex_1().min_h_0().into_any_element()
        };

        v_flex()
            .size_full()
            .child(header)
            .child(messages)
            .into_any_element()
    }

    fn render_message(
        &self,
        message: &MessageItem,
        now: OffsetDateTime,
        cx: &mut Context<'_, Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let date = local(message.date);
        let id = message.id.clone();
        let toggle = cx.listener(move |this, _, _, cx| this.toggle(&id, cx));
        let card = v_flex()
            .w_full()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .bg(theme.background);

        let sender = div()
            .flex_none()
            .when_unread(message.unread)
            .child(message.sender.clone());

        if !message.expanded {
            return card
                .child(
                    h_flex()
                        .id(SharedString::from(format!("message-{}", message.id)))
                        .px_4()
                        .py_2()
                        .gap_3()
                        .cursor_pointer()
                        .hover(|style| style.bg(theme.list_hover))
                        .on_click(toggle)
                        .child(sender.max_w(px(220.)).truncate())
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_role(TextRole::Secondary)
                                .text_color(theme.muted_foreground)
                                .child(message.snippet.clone()),
                        )
                        .child(date_text(format_date(date, now), cx)),
                )
                .into_any_element();
        }

        let header = h_flex()
            .id(SharedString::from(format!("message-{}", message.id)))
            .px_4()
            .pt_3()
            .pb_2()
            .gap_3()
            .items_start()
            .cursor_pointer()
            .on_click(toggle)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(sender.font_weight(FontWeight::SEMIBOLD))
                    .child(
                        div()
                            .truncate()
                            .text_role(TextRole::Secondary)
                            .text_color(theme.muted_foreground)
                            .child(message.recipients.clone()),
                    ),
            )
            .child(date_text(format_date(date, now), cx));

        let participants = v_flex()
            .px_4()
            .pb_3()
            .gap_0p5()
            .text_role(TextRole::Secondary)
            .child(participant("from", &message.from, cx))
            .children((!message.to.is_empty()).then(|| participant("to", &message.to, cx)))
            .children((!message.cc.is_empty()).then(|| participant("cc", &message.cc, cx)))
            .child(participant("date", &full_date(date), cx));

        let body = match &message.body {
            BodyState::NotLoaded | BodyState::Loading => h_flex()
                .gap_2()
                .text_role(TextRole::Secondary)
                .text_color(theme.muted_foreground)
                .child(Spinner::new().small())
                .child("Loading message…")
                .into_any_element(),
            BodyState::Loaded(body) => div()
                .text_role(TextRole::Body)
                .child(body.text.clone())
                .into_any_element(),
            BodyState::Unavailable(reason) => div()
                .text_role(TextRole::Secondary)
                .text_color(theme.muted_foreground)
                .child(reason.clone())
                .into_any_element(),
        };

        card.child(header)
            .child(participants)
            .child(
                div()
                    .px_4()
                    .pt_3()
                    .pb_4()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(body),
            )
            .into_any_element()
    }
}

/// Bold for unread messages, as Gmail shows them.
trait UnreadExt: gpui::Styled + Sized {
    fn when_unread(self, unread: bool) -> Self {
        if unread {
            self.font_weight(FontWeight::BOLD)
        } else {
            self
        }
    }
}

impl<E: gpui::Styled> UnreadExt for E {}

fn date_text(text: String, cx: &App) -> impl IntoElement {
    div()
        .flex_none()
        .text_role(TextRole::Caption)
        .text_color(cx.theme().muted_foreground)
        .child(text)
}

/// One line of an expanded message's participants: "to  Ada <ada@…>".
fn participant(name: &'static str, value: &str, cx: &App) -> impl IntoElement {
    h_flex()
        .items_start()
        .gap_2()
        .child(
            div()
                .w(px(36.))
                .flex_none()
                .text_color(cx.theme().muted_foreground)
                .child(name),
        )
        .child(div().flex_1().min_w_0().child(value.to_owned()))
}

fn label_chip(chip: &LabelChip) -> AnyElement {
    let tag = match chip.color.as_ref().and_then(chip_colors) {
        Some((background, foreground)) => Tag::custom(background, foreground, background),
        None => Tag::secondary(),
    };
    tag.small().child(chip.name.clone()).into_any_element()
}

/// A Gmail label color (`#rrggbb` hex) as background and text colors.
fn chip_colors(color: &LabelColor) -> Option<(Hsla, Hsla)> {
    Some((
        hex_color(&color.background_color)?,
        hex_color(&color.text_color)?,
    ))
}

fn hex_color(hex: &str) -> Option<Hsla> {
    parse_hex_color(hex).map(|value| rgb(value).into())
}

impl Render for ConversationView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        // The webview is created on the first frame that has a document for
        // it, so whether it is available is known before laying out.
        self.body.update(cx, |body, cx| body.prepare(window, cx));
        let theme = cx.theme();
        let pane = v_flex().size_full().min_w_0().bg(theme.background);
        let content = match &self.pane {
            ReadingPane::Empty => placeholder(
                "No conversation selected",
                Some("Choose a conversation from the list to read it here.".into()),
                cx,
            )
            .into_any_element(),
            ReadingPane::Loading(_) => loading().into_any_element(),
            ReadingPane::Missing(_) => placeholder(
                "This conversation is no longer available.",
                Some("It may have been deleted, or not synced yet.".into()),
                cx,
            )
            .into_any_element(),
            ReadingPane::Failed(_, error) => placeholder(
                "This conversation couldn't be loaded.",
                Some(error.clone().into()),
                cx,
            )
            .child(
                Button::new("retry-conversation")
                    .outline()
                    .small()
                    .label("Try again")
                    .on_click(cx.listener(|this, _, _, cx| this.retry(cx))),
            )
            .into_any_element(),
            ReadingPane::Open(conversation) => self.render_conversation(conversation, cx),
        };
        pane.child(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_colors_parse_from_gmail_hex() {
        let colors = chip_colors(&LabelColor {
            text_color: "#ffffff".to_owned(),
            background_color: "#16a765".to_owned(),
        });
        let (background, foreground) = colors.expect("valid colors");
        assert_eq!(foreground, Hsla::from(rgb(0xffffff)));
        assert_eq!(background, Hsla::from(rgb(0x16a765)));
        assert!(
            chip_colors(&LabelColor {
                text_color: "#fff".to_owned(),
                background_color: "#16a765".to_owned(),
            })
            .is_none()
        );
    }
}
