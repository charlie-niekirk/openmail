//! The reading pane: the open conversation's header, then its messages,
//! collapsed to a line or expanded to participants and plain-text body. The
//! state is a [`Conversation`]; this view loads it and keeps it current.

use gpui::{
    AnyElement, App, Context, Entity, FontWeight, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Task, Window, div, px, rgb,
};
use gpui_component::scroll::ScrollableElement as _;
use gpui_component::spinner::Spinner;
use gpui_component::tag::Tag;
use gpui_component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use openmail_core::{Label, LabelColor, MessageId, ThreadId};
use openmail_store::ThreadDetail;
use time::OffsetDateTime;

use crate::bridge::{AppEvent, AppModel};
use crate::conversation::{BodyState, Conversation, LabelChip, MessageItem};
use crate::sidebar::parse_hex_color;
use crate::state::Session;
use crate::thread_list::format_date;
use crate::time_format::{full_date, local};
use crate::typography::{TextRole, TypographyExt as _};

/// Widest a conversation's text runs, for readable lines on wide windows.
const MAX_WIDTH: gpui::Pixels = px(860.);

/// What the reading pane shows.
enum State {
    /// No thread is open.
    Empty,
    Loading(ThreadId),
    /// The thread is not in the store (deleted, or not synced).
    Missing(ThreadId),
    Failed(ThreadId, String),
    Open(Conversation),
}

impl State {
    fn thread_id(&self) -> Option<&ThreadId> {
        match self {
            Self::Empty => None,
            Self::Loading(id) | Self::Missing(id) | Self::Failed(id, _) => Some(id),
            Self::Open(conversation) => Some(&conversation.thread_id),
        }
    }
}

/// The conversation view. Open a thread with [`ConversationView::open_thread`].
pub struct ConversationView {
    model: Entity<AppModel>,
    state: State,
    /// The account the open thread belongs to.
    account: Option<String>,
    /// Loading the thread (replacing it cancels the previous load).
    load: Option<Task<()>>,
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
            state: State::Empty,
            account: None,
            load: None,
            _subscriptions: [events, status],
        };
        view.status_changed(cx);
        view
    }

    /// Opens `id`, replacing the open thread.
    pub fn open_thread(&mut self, id: ThreadId, cx: &mut Context<'_, Self>) {
        if self.state.thread_id() == Some(&id) {
            return;
        }
        self.state = State::Loading(id);
        self.reload(cx);
        cx.notify();
    }

    /// Closes the open thread, if any.
    pub fn close(&mut self, cx: &mut Context<'_, Self>) {
        if matches!(self.state, State::Empty) {
            return;
        }
        self.state = State::Empty;
        self.load = None;
        cx.notify();
    }

    /// The open thread, if any.
    pub fn thread_id(&self) -> Option<&ThreadId> {
        self.state.thread_id()
    }

    /// The signed-in account, if the mailbox is open.
    fn current_account(&self, cx: &App) -> Option<String> {
        match self.model.read(cx).state().session() {
            Session::Ready { account } => Some(account.clone()),
            _ => None,
        }
    }

    /// Follows the session: a different account closes the open thread, and
    /// a thread that waited for the mail service loads once it runs.
    fn status_changed(&mut self, cx: &mut Context<'_, Self>) {
        let account = self.current_account(cx);
        if account != self.account {
            self.account = account;
            self.state = State::Empty;
            self.load = None;
            cx.notify();
        }
        if matches!(self.state, State::Loading(_)) && self.load.is_none() {
            self.reload(cx);
        }
    }

    /// Queries the open thread (and the labels, for its chips) again.
    fn reload(&mut self, cx: &mut Context<'_, Self>) {
        let Some(id) = self.state.thread_id().cloned() else {
            return;
        };
        let Some(mail) = self.model.read(cx).mail().cloned() else {
            self.load = None;
            return;
        };
        self.load = Some(cx.spawn(async move |this, cx| {
            let detail = mail.thread(id.clone()).await;
            let labels = mail.labels().await.unwrap_or_default();
            this.update(cx, |this, cx| this.loaded(&id, detail, &labels, cx))
                .ok();
        }));
    }

    fn loaded(
        &mut self,
        id: &ThreadId,
        detail: openmail_sync::Result<Option<ThreadDetail>>,
        labels: &[Label],
        cx: &mut Context<'_, Self>,
    ) {
        if self.state.thread_id() != Some(id) {
            return;
        }
        let me = self.account.as_deref();
        match (detail, &mut self.state) {
            (Ok(Some(detail)), State::Open(conversation)) => {
                conversation.refresh(detail, labels, me);
            }
            (Ok(Some(detail)), _) => {
                self.state = State::Open(Conversation::new(detail, labels, me));
            }
            (Ok(None), _) => self.state = State::Missing(id.clone()),
            // A failed refresh keeps showing what was loaded.
            (Err(_), State::Open(_)) => {}
            (Err(error), _) => self.state = State::Failed(id.clone(), error.to_string()),
        }
        self.load_bodies(cx);
        cx.notify();
    }

    /// Requests the bodies of expanded messages that have none.
    fn load_bodies(&mut self, cx: &mut Context<'_, Self>) {
        let State::Open(conversation) = &mut self.state else {
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
                    if let State::Open(conversation) = &mut this.state {
                        match body {
                            Ok(body) => conversation.body_loaded(&id, body.as_ref()),
                            Err(error) => conversation.body_failed(&id, &error.to_string()),
                        }
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        }
    }

    fn body_fetched(&mut self, id: &MessageId, cx: &mut Context<'_, Self>) {
        if let State::Open(conversation) = &mut self.state
            && conversation.body_fetched(id)
        {
            self.load_bodies(cx);
            cx.notify();
        }
    }

    fn toggle(&mut self, id: &MessageId, cx: &mut Context<'_, Self>) {
        if let State::Open(conversation) = &mut self.state {
            conversation.toggle(id);
            self.load_bodies(cx);
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
            .pb_2()
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

        let messages = conversation
            .messages
            .iter()
            .map(|message| self.render_message(message, now, cx));

        v_flex()
            .id(SharedString::from(format!(
                "conversation-{}",
                conversation.thread_id
            )))
            .size_full()
            .overflow_y_scrollbar()
            .child(
                v_flex()
                    .w_full()
                    .max_w(MAX_WIDTH)
                    .mx_auto()
                    .px_6()
                    .py_5()
                    .gap_3()
                    .child(header)
                    .children(messages),
            )
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
            BodyState::Loaded(text) => div()
                .text_role(TextRole::Body)
                .child(text.clone())
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = cx.theme();
        let pane = v_flex().size_full().min_w_0().bg(theme.background);
        let centered = |text: &str, cx: &App| {
            v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .px_8()
                .text_center()
                .text_role(TextRole::Secondary)
                .text_color(cx.theme().muted_foreground)
                .child(text.to_owned())
                .into_any_element()
        };
        let content = match &self.state {
            State::Empty => centered("No conversation selected", cx),
            State::Loading(_) => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .child(Spinner::new())
                .into_any_element(),
            State::Missing(_) => centered("This conversation is no longer available.", cx),
            State::Failed(_, error) => centered(
                &format!("This conversation couldn't be loaded: {error}"),
                cx,
            ),
            State::Open(conversation) => self.render_conversation(conversation, cx),
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
