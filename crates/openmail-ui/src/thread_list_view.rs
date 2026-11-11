//! The thread list: a virtualized `uniform_list` of a label's threads that
//! loads more as it scrolls, refreshes on `ThreadsChanged` without moving
//! what is on screen, and reports the thread the user selects.
//!
//! The logic is in the [`ThreadList`] view-model; this view runs its queries
//! through the [`MailHandle`](openmail_sync::MailHandle) and renders its rows.

use std::ops::Range;

use gpui::{
    App, Context, Entity, EventEmitter, FontWeight, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, Pixels, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Task, UniformListScrollHandle, Window, div, prelude::FluentBuilder as _, px, rgb,
    uniform_list,
};
use gpui_component::scroll::ScrollableElement as _;
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use openmail_core::{LabelColor, LabelId, ThreadId};
use openmail_store::ThreadView;
use time::{OffsetDateTime, UtcOffset};

use crate::assets::IMPORTANCE_ICON;
use crate::bridge::{AppEvent, AppModel};
use crate::state::Session;
use crate::thread_list::{LabelChip, Query, QueryKind, ThreadList, ThreadRow};
use crate::typography::{TextRole, TypographyExt as _};

/// The height of every row (`uniform_list` needs them all the same).
pub const ROW_HEIGHT: Pixels = px(76.);

/// What the thread list tells its parent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadListEvent {
    /// The user selected a thread.
    Selected(ThreadId),
}

/// The thread on top of the viewport and how far it is scrolled past, kept
/// in place across a refresh.
struct Anchor {
    id: ThreadId,
    within: Pixels,
}

/// The thread list of one label (the Inbox until [`set_label`] says
/// otherwise).
///
/// [`set_label`]: ThreadListView::set_label
pub struct ThreadListView {
    model: Entity<AppModel>,
    list: ThreadList,
    scroll: UniformListScrollHandle,
    /// The account whose threads are shown.
    account: Option<String>,
    /// The query running, if any (the view-model runs one at a time).
    query: Option<Task<()>>,
    labels: Option<Task<()>>,
    _subscriptions: [Subscription; 2],
}

impl EventEmitter<ThreadListEvent> for ThreadListView {}

impl ThreadListView {
    /// Creates the list. It loads once the mail service is running, and
    /// refreshes when threads or labels change.
    pub fn new(cx: &mut Context<'_, Self>) -> Self {
        let model = AppModel::global(cx);
        let events = cx.subscribe(&model, |this, _, event: &AppEvent, cx| match event {
            AppEvent::ThreadsChanged => this.refresh(cx),
            AppEvent::LabelsChanged => this.load_labels(cx),
            AppEvent::BodyFetched(_) => {}
        });
        let status = cx.observe(&model, |this, _, cx| this.connect(cx));
        let mut this = Self {
            model,
            list: ThreadList::default(),
            scroll: UniformListScrollHandle::new(),
            account: None,
            query: None,
            labels: None,
            _subscriptions: [events, status],
        };
        this.connect(cx);
        this
    }

    /// Shows the threads with `label`.
    pub fn set_label(&mut self, label: LabelId, cx: &mut Context<'_, Self>) {
        self.set_view(ThreadView::Label(label), cx);
    }

    /// Shows the threads of `view`, from the top, with nothing selected.
    pub fn set_view(&mut self, view: ThreadView, cx: &mut Context<'_, Self>) {
        if let Some(query) = self.list.set_view(view) {
            self.scroll_to_top();
            self.run(query, cx);
            cx.notify();
        }
    }

    /// The selected thread, if any.
    pub fn selected(&self) -> Option<&ThreadId> {
        self.list.selected()
    }

    /// Starts over when an account's service starts (or another account's).
    fn connect(&mut self, cx: &mut Context<'_, Self>) {
        let model = self.model.read(cx);
        let account = match model.state().session() {
            Session::Ready { account } if model.mail().is_some() => Some(account.clone()),
            _ => None,
        };
        if account == self.account {
            return;
        }
        self.account = account.clone();
        if account.is_none() {
            return;
        }
        let mut list = ThreadList::new(self.list.view().clone());
        list.set_account(account);
        self.list = list;
        self.query = None;
        self.scroll_to_top();
        self.load_labels(cx);
        self.refresh(cx);
        cx.notify();
    }

    fn refresh(&mut self, cx: &mut Context<'_, Self>) {
        if self.account.is_none() {
            return;
        }
        if let Some(query) = self.list.refresh() {
            self.run(query, cx);
        }
    }

    /// Fetches a page in the background and applies it.
    fn run(&mut self, query: Query, cx: &mut Context<'_, Self>) {
        let Some(mail) = self.model.read(cx).mail().cloned() else {
            self.list
                .finish(&query, Err("Openmail isn't connected".to_owned()));
            return;
        };
        self.query = Some(cx.spawn(async move |this, cx| {
            let result = mail
                .thread_page(query.view.clone(), query.after.clone(), query.limit)
                .await
                .map_err(|err| err.to_string());
            this.update(cx, |this, cx| this.finished(&query, result, cx))
                .ok();
        }));
    }

    fn finished(
        &mut self,
        query: &Query,
        result: Result<openmail_store::ThreadPage, String>,
        cx: &mut Context<'_, Self>,
    ) {
        let anchor = match query.kind {
            QueryKind::Refresh => self.anchor(),
            QueryKind::Reset | QueryKind::More => None,
        };
        let finished = self.list.finish(query, result);
        if finished.applied {
            if let Some(anchor) = anchor {
                self.restore(&anchor);
            }
            cx.notify();
        }
        if let Some(follow_up) = finished.follow_up {
            self.run(follow_up, cx);
        }
    }

    fn load_labels(&mut self, cx: &mut Context<'_, Self>) {
        let Some(mail) = self.model.read(cx).mail().cloned() else {
            return;
        };
        self.labels = Some(cx.spawn(async move |this, cx| {
            // Without labels, rows just have no chips.
            if let Ok(labels) = mail.labels().await {
                this.update(cx, |this, cx| {
                    this.list.set_labels(labels);
                    cx.notify();
                })
                .ok();
            }
        }));
    }

    fn select(&mut self, id: ThreadId, cx: &mut Context<'_, Self>) {
        if self.list.select(id.clone()) {
            cx.emit(ThreadListEvent::Selected(id));
            cx.notify();
        }
    }

    /// The row on top of the viewport, unless the list is scrolled to the
    /// very top, where new threads should come into view.
    fn anchor(&self) -> Option<Anchor> {
        let scrolled = -self.scroll.0.borrow().base_handle.offset().y;
        if scrolled <= px(0.) {
            return None;
        }
        let ix = (scrolled / ROW_HEIGHT).floor();
        let within = scrolled - ROW_HEIGHT * ix;
        let id = self.list.thread(ix as usize)?.id.clone();
        Some(Anchor { id, within })
    }

    /// Scrolls the anchor's row back to where it was. If it is gone, the
    /// list stays where it is.
    fn restore(&self, anchor: &Anchor) {
        if let Some(ix) = self.list.position(&anchor.id) {
            let top = ROW_HEIGHT * ix as f32 + anchor.within;
            let handle = &self.scroll.0.borrow().base_handle;
            let mut offset = handle.offset();
            offset.y = -top;
            handle.set_offset(offset);
        }
    }

    fn scroll_to_top(&self) {
        let handle = &self.scroll.0.borrow().base_handle;
        handle.set_offset(Default::default());
    }

    fn rows(&mut self, range: Range<usize>, cx: &mut Context<'_, Self>) -> Vec<gpui::AnyElement> {
        if let Some(query) = self.list.load_more(range.end) {
            self.run(query, cx);
        }
        let now = local(OffsetDateTime::now_utc());
        range
            .filter_map(|ix| self.list.row(ix, now, local))
            .map(|row| self.row(row, cx).into_any_element())
            .collect()
    }

    fn row(&self, row: ThreadRow, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = cx.theme();
        let weight = if row.unread {
            FontWeight::BOLD
        } else {
            FontWeight::NORMAL
        };
        let muted = theme.muted_foreground;
        let marker = theme.warning;
        let id = row.id.clone();

        v_flex()
            .id(SharedString::from(row.id.as_str().to_owned()))
            .w_full()
            .h(ROW_HEIGHT)
            .overflow_hidden()
            .px_3()
            .justify_center()
            .border_b_1()
            .border_color(theme.border)
            .cursor_pointer()
            .map(|el| {
                if row.selected {
                    el.bg(theme.colors.list_active)
                } else {
                    el.hover(|style| style.bg(theme.colors.list_hover))
                }
            })
            .on_click(cx.listener(move |this, _, _, cx| this.select(id.clone(), cx)))
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap_2()
                    .text_role(TextRole::Body)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_weight(weight)
                            .child(row.senders),
                    )
                    .when(row.attachment, |el| {
                        el.child(
                            Icon::new(gpui_kit_assets::IconName::Paperclip)
                                .xsmall()
                                .text_color(muted),
                        )
                    })
                    .child(
                        div()
                            .flex_none()
                            .text_role(TextRole::Caption)
                            .font_weight(weight)
                            .when(!row.unread, |el| el.text_color(muted))
                            .child(row.date),
                    ),
            )
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap_1()
                    .text_role(TextRole::Body)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_weight(weight)
                            .child(row.subject),
                    )
                    .children(row.labels.iter().map(|chip| label_chip(chip, cx)))
                    .when(row.more_labels > 0, |el| {
                        el.child(chip(format!("+{}", row.more_labels), None, cx).text_color(muted))
                    })
                    .when(row.important, |el| {
                        el.child(
                            Icon::empty()
                                .path(IMPORTANCE_ICON)
                                .xsmall()
                                .text_color(marker),
                        )
                    })
                    .when(row.starred, |el| {
                        el.child(Icon::new(IconName::StarFill).xsmall().text_color(marker))
                    }),
            )
            .child(
                div()
                    .text_role(TextRole::Secondary)
                    .text_color(muted)
                    .truncate()
                    .child(row.snippet),
            )
    }
}

fn label_chip(label: &LabelChip, cx: &App) -> gpui::Div {
    chip(label.name.clone(), label.color.as_ref(), cx)
}

/// A small rounded label, in Gmail's colors for it if there are any.
fn chip(text: String, color: Option<&LabelColor>, cx: &App) -> gpui::Div {
    let theme = cx.theme();
    let (background, foreground) = color
        .and_then(|color| Some((hex(&color.background_color)?, hex(&color.text_color)?)))
        .unwrap_or((theme.secondary, theme.secondary_foreground));
    div()
        .flex_none()
        .max_w(px(96.))
        .px_1p5()
        .rounded_sm()
        .bg(background)
        .text_color(foreground)
        .text_role(TextRole::Caption)
        .truncate()
        .child(text)
}

/// A `#rrggbb` color.
fn hex(color: &str) -> Option<Hsla> {
    let digits = color.strip_prefix('#')?;
    if digits.len() != 6 {
        return None;
    }
    u32::from_str_radix(digits, 16).ok().map(|n| rgb(n).into())
}

/// `date` in the local time zone at that date (so dates across a daylight
/// saving change are right), or unchanged if the zone can't be read.
fn local(date: OffsetDateTime) -> OffsetDateTime {
    use chrono::{Offset as _, TimeZone as _};

    chrono::Local
        .timestamp_opt(date.unix_timestamp(), 0)
        .single()
        .and_then(|local| {
            UtcOffset::from_whole_seconds(local.offset().fix().local_minus_utc()).ok()
        })
        .map_or(date, |offset| date.to_offset(offset))
}

impl Render for ThreadListView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .size_full()
            .bg(theme.colors.list)
            .child(
                uniform_list(
                    "thread-list",
                    self.list.len(),
                    cx.processor(|this, range: Range<usize>, _window, cx| this.rows(range, cx)),
                )
                .track_scroll(&self.scroll)
                .size_full(),
            )
            .vertical_scrollbar(&self.scroll)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gmail_label_colors() {
        assert_eq!(hex("#ffffff"), Some(rgb(0xffffff).into()));
        assert_eq!(hex("#16a765"), Some(rgb(0x16a765).into()));
        assert_eq!(hex("16a765"), None);
        assert_eq!(hex("#fff"), None);
        assert_eq!(hex("#zzzzzz"), None);
    }

    #[test]
    fn rows_fit_their_three_lines() {
        let lines = TextRole::Body.line_height() * 2. + TextRole::Secondary.line_height();
        assert!(ROW_HEIGHT >= lines + px(12.));
    }
}
