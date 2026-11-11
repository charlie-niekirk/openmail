//! The sidebar: mailboxes and labels, drawn from the [`Sidebar`] view-model.
//!
//! [`SidebarView`] queries labels and counts through the mail service when the
//! service starts and again on [`AppEvent::LabelsChanged`] or
//! [`AppEvent::ThreadsChanged`], and emits [`SidebarEvent::Selected`] when the
//! selected mailbox changes.

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, ClickEvent, Context, ElementId, Entity, EventEmitter, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement as _, Render,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, div, px, rgb,
};
use gpui_component::{ActiveTheme as _, h_flex, v_flex};

use crate::bridge::{AppEvent, AppModel};
use crate::sidebar::{Counts, Group, Mailbox, MailboxRow, Row, Sidebar, parse_hex_color};
use crate::typography::{TextRole, TypographyExt as _};

/// Indentation per level of nesting.
const INDENT: gpui::Pixels = px(14.);

/// Width of the expand/collapse arrow's column.
const ARROW_WIDTH: gpui::Pixels = px(14.);

/// What [`SidebarView`] tells its subscribers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidebarEvent {
    /// The user selected another mailbox, or the selected label was deleted
    /// and the Inbox is selected instead.
    Selected(Mailbox),
}

/// The sidebar view.
pub struct SidebarView {
    model: Entity<AppModel>,
    sidebar: Sidebar,
    /// Whether labels were queried from the current mail service.
    loaded: bool,
    /// The running query; replacing it cancels the previous one.
    query: Option<Task<()>>,
    _subscriptions: [Subscription; 2],
}

impl EventEmitter<SidebarEvent> for SidebarView {}

impl SidebarView {
    /// Creates the view, with the Inbox selected. It queries the labels once
    /// the mail service is running.
    pub fn new(cx: &mut Context<'_, Self>) -> Self {
        let model = AppModel::global(cx);
        let events = cx.subscribe(&model, |this, _, event: &AppEvent, cx| match event {
            AppEvent::LabelsChanged | AppEvent::ThreadsChanged => this.refresh(cx),
            AppEvent::BodyFetched(_) => {}
        });
        let service = cx.observe(&model, |this, model, cx| {
            if model.read(cx).mail().is_none() {
                this.loaded = false;
                this.query = None;
            } else if !this.loaded {
                this.refresh(cx);
            }
        });
        let mut this = Self {
            model,
            sidebar: Sidebar::new(),
            loaded: false,
            query: None,
            _subscriptions: [events, service],
        };
        this.refresh(cx);
        this
    }

    /// The selected mailbox.
    pub fn selected(&self) -> &Mailbox {
        self.sidebar.selected()
    }

    /// Selects a mailbox, telling subscribers if the selection changed.
    pub fn select(&mut self, mailbox: Mailbox, cx: &mut Context<'_, Self>) {
        if self.sidebar.select(mailbox.clone()) {
            cx.emit(SidebarEvent::Selected(mailbox));
            cx.notify();
        }
    }

    fn toggle(&mut self, group: Group, cx: &mut Context<'_, Self>) {
        self.sidebar.toggle(group);
        cx.notify();
    }

    /// Queries the labels and their counts again. Both are quick store reads,
    /// so it does both on every change rather than tracking which is stale.
    fn refresh(&mut self, cx: &mut Context<'_, Self>) {
        let Some(mail) = self.model.read(cx).mail().cloned() else {
            return;
        };
        self.loaded = true;
        self.query = Some(cx.spawn(async move |this, cx| {
            let labels = mail.labels().await;
            let counts = mail.label_counts().await;
            // If the service stopped meanwhile, keep showing what we had: the
            // window leaves the mailbox once the session ends.
            let (Ok(labels), Ok(counts)) = (labels, counts) else {
                return;
            };
            let counts = counts.into_iter().map(|counts| {
                let id = counts.label_id;
                let counts = Counts {
                    total: counts.threads_total,
                    unread: counts.threads_unread,
                };
                (id, counts)
            });
            this.update(cx, |this, cx| {
                this.sidebar.set_counts(counts);
                if this.sidebar.set_labels(labels) {
                    cx.emit(SidebarEvent::Selected(this.sidebar.selected().clone()));
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn mailbox_row(&self, ix: usize, row: MailboxRow, cx: &Context<'_, Self>) -> AnyElement {
        let theme = cx.theme();
        let (background, foreground) = if row.selected {
            (theme.sidebar_accent, theme.sidebar_accent_foreground)
        } else {
            (gpui::transparent_black(), theme.sidebar_foreground)
        };
        let weight = if row.unread || row.selected {
            FontWeight::SEMIBOLD
        } else {
            FontWeight::NORMAL
        };

        let arrow = match (row.expanded, row.mailbox.label()) {
            (Some(expanded), Some(id)) => {
                let group = Group::Label(id.clone());
                arrow(("sidebar-arrow", ix), expanded, cx)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        this.toggle(group.clone(), cx);
                    }))
                    .into_any_element()
            }
            // Keeps the names aligned with those of rows that have an arrow.
            _ => div().flex_none().w(ARROW_WIDTH).into_any_element(),
        };
        let swatch = row.color.as_ref().map(|color| {
            let fill = parse_hex_color(&color.background_color)
                .map_or(theme.muted_foreground, |hex| gpui::Hsla::from(rgb(hex)));
            div().flex_none().size(px(10.)).rounded_full().bg(fill)
        });

        let mailbox = row.mailbox.clone();
        row_frame(("sidebar-row", ix), row.depth, cx)
            .bg(background)
            .when(!row.selected, |frame| {
                frame.hover(|style| style.bg(theme.list_hover))
            })
            .text_color(foreground)
            .font_weight(weight)
            .child(arrow)
            .children(swatch)
            .child(div().flex_1().min_w_0().truncate().child(row.name))
            .children(row.count.map(|count| {
                div()
                    .flex_none()
                    .text_role(TextRole::Caption)
                    .font_weight(weight)
                    .child(count.to_string())
            }))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.select(mailbox.clone(), cx);
            }))
            .into_any_element()
    }

    fn categories_row(&self, ix: usize, expanded: bool, cx: &Context<'_, Self>) -> AnyElement {
        let theme = cx.theme();
        row_frame(("sidebar-row", ix), 0, cx)
            .hover(|style| style.bg(theme.list_hover))
            .text_color(theme.sidebar_foreground)
            .child(arrow(("sidebar-arrow", ix), expanded, cx))
            .child(div().flex_1().child("Categories"))
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.toggle(Group::Categories, cx);
            }))
            .into_any_element()
    }
}

/// A clickable row, indented to `depth`. Callers add the hover background,
/// which the selected row does without.
fn row_frame(id: impl Into<ElementId>, depth: usize, cx: &App) -> gpui::Stateful<gpui::Div> {
    let theme = cx.theme();
    h_flex()
        .id(id)
        .flex_none()
        .h(px(30.))
        .gap_2()
        .pl(px(8.) + INDENT * depth as f32)
        .pr_3()
        .rounded(theme.radius)
        .cursor_pointer()
}

/// The arrow that expands or collapses a group.
fn arrow(id: impl Into<ElementId>, expanded: bool, cx: &App) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .w(ARROW_WIDTH)
        .text_role(TextRole::Caption)
        .text_color(cx.theme().muted_foreground)
        .child(if expanded { "▾" } else { "▸" })
}

impl Render for SidebarView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = cx.theme();
        let rows: Vec<AnyElement> = self
            .sidebar
            .rows()
            .into_iter()
            .enumerate()
            .map(|(ix, row)| match row {
                Row::Mailbox(row) => self.mailbox_row(ix, row, cx),
                Row::Categories { expanded } => self.categories_row(ix, expanded, cx),
                Row::Heading(heading) => div()
                    .flex_none()
                    .pt_4()
                    .pb_1()
                    .px_2()
                    .text_role(TextRole::Caption)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.muted_foreground)
                    .child(heading)
                    .into_any_element(),
            })
            .collect();

        v_flex()
            .id("sidebar")
            .size_full()
            .overflow_y_scroll()
            .p_2()
            .text_role(TextRole::Body)
            .children(rows)
    }
}
