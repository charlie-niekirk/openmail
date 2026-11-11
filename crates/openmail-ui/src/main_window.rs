//! The main window: the frame (a transparent macOS title bar showing the
//! account and sync status) around either the mailbox (the sidebar, the
//! thread list, then the reading pane, wired together by [`panes`] and
//! separated by dividers that resize them), the onboarding
//! screens (sign-in, then a new account's first sync), or a notice when the
//! mailbox could not be opened.

use std::path::PathBuf;
use std::time::Duration;

use gpui::{
    App, AppContext as _, Axis, Bounds, Context, DragMoveEvent, Entity, InteractiveElement as _,
    IntoElement, ParentElement as _, Pixels, Render, SharedString, Styled as _, Subscription, Task,
    Window, WindowBounds, WindowOptions, div, px, size,
};
use gpui_component::resizable::{HandleEdge, resize_handle};
use gpui_component::{ActiveTheme as _, TitleBar, h_flex, v_flex};

use crate::bridge::AppModel;
use crate::conversation_view::ConversationView;
use crate::onboarding::{Setup, SignIn};
use crate::onboarding_view::{setup_screen, sign_in_screen};
use crate::pane_sizes::{Divider, PaneSizes, layout_path};
use crate::panes::{self, PaneCommand};
use crate::sidebar_view::{SidebarEvent, SidebarView};
use crate::state::{AppState, Session};
use crate::thread_list_view::{ThreadListEvent, ThreadListView};
use crate::typography::{TextRole, TypographyExt as _};

/// The window title shown by the OS (Window menu, Mission Control, task bar).
pub const WINDOW_TITLE: &str = "Openmail";

/// Size of a newly opened main window.
pub const DEFAULT_SIZE: gpui::Size<Pixels> = size(px(1200.), px(780.));

/// Smallest size the main window can be resized to while all three panes fit.
pub const MIN_SIZE: gpui::Size<Pixels> = size(px(760.), px(480.));

/// How long after a divider last moved its pane sizes are saved.
const SAVE_DELAY: Duration = Duration::from_millis(300);

/// Window options for the main window: a transparent macOS title bar with traffic
/// lights (drawn by `gpui-component`'s [`TitleBar`]), titled "Openmail".
pub fn main_window_options(bounds: Bounds<Pixels>) -> WindowOptions {
    let mut titlebar = TitleBar::title_bar_options();
    titlebar.title = Some(WINDOW_TITLE.into());

    WindowOptions {
        titlebar: Some(titlebar),
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        window_min_size: Some(MIN_SIZE),
        app_id: Some("openmail".into()),
        ..TitleBar::window_options()
    }
}

/// What the window shows below the title bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    /// The three panes (empty while the mailbox opens).
    Mailbox,
    /// The sign-in screen.
    SignIn(SignIn),
    /// A newly signed-in account's first sync, until its inbox is ready.
    Setup { account: String, setup: Setup },
    /// A message in place of the mailbox.
    Notice { title: String, detail: String },
}

impl Content {
    pub fn for_state(state: &AppState) -> Self {
        match state.session() {
            Session::Starting => Self::Mailbox,
            Session::Ready { account } if state.is_setting_up() => Self::Setup {
                account: account.clone(),
                setup: Setup::for_sync(state.sync()),
            },
            Session::Ready { .. } => Self::Mailbox,
            Session::SignedOut(sign_in) => Self::SignIn(sign_in.clone()),
            Session::Failed { error } => Self::Notice {
                title: "Openmail couldn't open your mailbox".to_owned(),
                detail: error.clone(),
            },
        }
    }
}

/// What a divider drag carries: the divider. GPUI draws it as nothing.
#[derive(Clone)]
struct DividerDrag(Divider);

impl Render for DividerDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<'_, Self>) -> impl IntoElement {
        gpui::Empty
    }
}

/// The root view of the main window.
pub struct MainWindow {
    model: Entity<AppModel>,
    sidebar: Entity<SidebarView>,
    thread_list: Entity<ThreadListView>,
    conversation: Entity<ConversationView>,
    /// The user's pane widths.
    panes: PaneSizes,
    /// Where they are saved, if there is a data directory.
    layout_path: Option<PathBuf>,
    /// Saving them, once the divider stops moving.
    save: Option<Task<()>>,
    _subscriptions: [Subscription; 4],
}

impl MainWindow {
    /// Creates the view. It re-renders when the session or sync status
    /// changes, keeps the theme in step with the system appearance, and
    /// passes each pane's selection on to the next.
    pub fn new(window: &mut Window, cx: &mut Context<'_, Self>) -> Self {
        let model = AppModel::global(cx);
        let appearance = crate::theme::follow_system_appearance(window, cx);
        let status = cx.observe(&model, |_, _, cx| cx.notify());
        let sidebar = cx.new(SidebarView::new);
        let thread_list = cx.new(ThreadListView::new);
        let conversation = cx.new(ConversationView::new);
        let mailbox = cx.subscribe(&sidebar, |this, _, event: &SidebarEvent, cx| {
            this.apply(panes::on_sidebar(event), cx);
        });
        let thread = cx.subscribe(&thread_list, |this, _, event: &ThreadListEvent, cx| {
            this.apply(panes::on_thread_list(event), cx);
        });
        // A few bytes, read once before the first frame so the panes open at
        // their saved widths rather than jumping to them.
        let layout_path = layout_path();
        let panes = layout_path
            .as_deref()
            .map(PaneSizes::load)
            .unwrap_or_default();
        let mut this = Self {
            model,
            sidebar,
            thread_list,
            conversation,
            panes,
            layout_path,
            save: None,
            _subscriptions: [appearance, status, mailbox, thread],
        };
        // Start the list on whatever the sidebar selected.
        let view = panes::thread_view(this.sidebar.read(cx).selected());
        this.apply(vec![PaneCommand::ShowThreads(view)], cx);
        this
    }

    /// Carries out what one pane's selection asks of the others.
    fn apply(&mut self, commands: Vec<PaneCommand>, cx: &mut Context<'_, Self>) {
        for command in commands {
            match command {
                PaneCommand::ShowThreads(view) => self
                    .thread_list
                    .update(cx, |list, cx| list.set_view(view, cx)),
                PaneCommand::OpenThread(id) => self
                    .conversation
                    .update(cx, |conversation, cx| conversation.open_thread(id, cx)),
                PaneCommand::CloseThread => self.conversation.update(cx, ConversationView::close),
            }
        }
    }

    /// Follows a divider being dragged: resizes its pane, then saves the
    /// sizes once it stops.
    fn drag_divider(
        &mut self,
        event: &DragMoveEvent<DividerDrag>,
        _: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let divider = event.drag(cx).0;
        let x = f32::from(event.event.position.x - event.bounds.left());
        let width = f32::from(event.bounds.size.width);
        if self.panes.drag(divider, x, width) {
            self.save_panes(cx);
            cx.notify();
        }
    }

    /// Saves the pane sizes after [`SAVE_DELAY`], unless they change again
    /// first (which replaces this save with a later one).
    fn save_panes(&mut self, cx: &mut Context<'_, Self>) {
        let Some(path) = self.layout_path.clone() else {
            return;
        };
        let panes = self.panes;
        self.save = Some(cx.spawn(async move |_, cx| {
            cx.background_executor().timer(SAVE_DELAY).await;
            let saved = cx.background_spawn(async move { panes.save(&path) }).await;
            if let Err(err) = saved {
                tracing::warn!("could not save the pane sizes: {err}");
            }
        }));
    }

    fn title_bar(state: &AppState, cx: &App) -> impl IntoElement {
        let theme = cx.theme();
        let account: Option<SharedString> = match state.session() {
            Session::Ready { account } => Some(account.clone().into()),
            _ => None,
        };
        TitleBar::new().child(
            h_flex()
                .w_full()
                .pr_3()
                .gap_3()
                .justify_between()
                .text_role(TextRole::Caption)
                .text_color(theme.muted_foreground)
                .child(
                    div()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child(account.unwrap_or_else(|| SharedString::new_static(WINDOW_TITLE))),
                )
                .children(state.status_text()),
        )
    }

    fn mailbox(&self, window: &Window, cx: &Context<'_, Self>) -> impl IntoElement {
        let theme = cx.theme();
        // The mailbox spans the window, so its width is the window's.
        let shown = self.panes.fit(f32::from(window.viewport_size().width));
        h_flex()
            .id("mailbox")
            .flex_1()
            .min_h_0()
            .on_drag_move(cx.listener(Self::drag_divider))
            .child(
                div()
                    .relative()
                    .h_full()
                    .w(px(shown.sidebar))
                    .flex_none()
                    .bg(theme.sidebar)
                    .child(self.sidebar.clone())
                    .child(divider(Divider::Sidebar)),
            )
            .child(
                div()
                    .relative()
                    .h_full()
                    .w(px(shown.thread_list))
                    .flex_none()
                    .child(self.thread_list.clone())
                    .child(divider(Divider::ThreadList)),
            )
            .child(
                div()
                    .h_full()
                    .flex_1()
                    .min_w_0()
                    .child(self.conversation.clone()),
            )
    }

    fn notice(title: String, detail: String, cx: &App) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .flex_1()
            .min_h_0()
            .items_center()
            .justify_center()
            .gap_2()
            .px_8()
            .child(div().text_role(TextRole::Title).child(title))
            .child(
                div()
                    .max_w(px(480.))
                    .text_center()
                    .text_role(TextRole::Body)
                    .text_color(theme.muted_foreground)
                    .child(detail),
            )
    }
}

/// The draggable divider on a pane's right edge.
fn divider(divider: Divider) -> impl IntoElement {
    let id = match divider {
        Divider::Sidebar => "sidebar-divider",
        Divider::ThreadList => "thread-list-divider",
    };
    resize_handle::<DividerDrag, DividerDrag>(id, Axis::Horizontal)
        .inside(HandleEdge::Trailing)
        .on_drag(DividerDrag(divider), |drag, _, _, cx| {
            cx.stop_propagation();
            cx.new(|_| drag.clone())
        })
}

impl Render for MainWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let state = self.model.read(cx).state();
        let theme = cx.theme();
        let content = match Content::for_state(state) {
            Content::Mailbox => self.mailbox(window, cx).into_any_element(),
            Content::SignIn(sign_in) => sign_in_screen(&sign_in, cx),
            Content::Setup { account, setup } => setup_screen(&account, &setup, cx),
            Content::Notice { title, detail } => Self::notice(title, detail, cx).into_any_element(),
        };

        v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .font_family(theme.font_family.clone())
            .text_role(TextRole::Body)
            .child(Self::title_bar(state, cx))
            .child(content)
    }
}

#[cfg(test)]
mod tests {
    use gpui::{WindowBounds, point};
    use openmail_sync::{Changes, MailEvent, SyncStatus};

    use super::*;
    use crate::state::SyncState;

    fn options() -> WindowOptions {
        main_window_options(Bounds::new(point(px(0.), px(0.)), DEFAULT_SIZE))
    }

    #[test]
    fn titlebar_is_transparent_and_titled() {
        let titlebar = options().titlebar.expect("titlebar options");
        assert!(titlebar.appears_transparent);
        assert!(titlebar.traffic_light_position.is_some());
        assert_eq!(titlebar.title.as_deref(), Some(WINDOW_TITLE));
    }

    #[test]
    fn opens_at_default_size_with_minimum() {
        let options = options();
        let Some(WindowBounds::Windowed(bounds)) = options.window_bounds else {
            panic!("expected windowed bounds");
        };
        assert_eq!(bounds.size, DEFAULT_SIZE);
        assert_eq!(options.window_min_size, Some(MIN_SIZE));
    }

    #[test]
    fn default_size_fits_the_default_panes() {
        let width = f32::from(DEFAULT_SIZE.width);
        assert_eq!(PaneSizes::default().fit(width), PaneSizes::default());
        assert!(DEFAULT_SIZE.width >= MIN_SIZE.width);
        assert!(DEFAULT_SIZE.height >= MIN_SIZE.height);
    }

    #[test]
    fn shows_the_mailbox_while_starting_and_when_ready() {
        let mut state = AppState::new();
        assert_eq!(Content::for_state(&state), Content::Mailbox);
        state.set_session(Session::Ready {
            account: "ada@example.com".to_owned(),
        });
        assert_eq!(Content::for_state(&state), Content::Mailbox);
    }

    #[test]
    fn signs_in_then_sets_up_a_new_account() {
        let mut state = AppState::new();
        let sign_in = SignIn::Ready { notice: None };
        state.set_session(Session::SignedOut(sign_in.clone()));
        assert_eq!(Content::for_state(&state), Content::SignIn(sign_in));

        state.begin_sign_in();
        assert_eq!(
            Content::for_state(&state),
            Content::SignIn(SignIn::InBrowser)
        );

        state.signed_in("ada@example.com".to_owned());
        let Content::Setup { account, setup } = Content::for_state(&state) else {
            panic!("expected the setup screen");
        };
        assert_eq!(account, "ada@example.com");
        assert_eq!(setup, Setup::for_sync(&SyncState::Waiting));

        state.apply(MailEvent::SyncStatus(SyncStatus::UpToDate(
            Changes::default(),
        )));
        assert_eq!(Content::for_state(&state), Content::Mailbox);
    }

    #[test]
    fn explains_why_there_is_no_mailbox() {
        let mut state = AppState::new();
        state.set_session(Session::Failed {
            error: "disk full".to_owned(),
        });
        let Content::Notice { title, detail } = Content::for_state(&state) else {
            panic!("expected a notice");
        };
        assert!(title.contains("couldn't open"));
        assert_eq!(detail, "disk full");
    }
}
