//! The main window: the frame (a transparent macOS title bar showing the
//! account and sync status) around either the mailbox (the sidebar, then the
//! thread list and reading pane, still placeholders), the onboarding
//! screens (sign-in, then a new account's first sync), or a notice when the
//! mailbox could not be opened.

use gpui::{
    App, AppContext as _, Bounds, Context, Entity, IntoElement, ParentElement as _, Pixels, Render,
    SharedString, Styled as _, Subscription, Window, WindowBounds, WindowOptions, div, px, size,
};
use gpui_component::{ActiveTheme as _, TitleBar, h_flex, v_flex};

use crate::bridge::AppModel;
use crate::conversation_view::ConversationView;
use crate::onboarding::{Setup, SignIn};
use crate::onboarding_view::{setup_screen, sign_in_screen};
use crate::sidebar_view::SidebarView;
use crate::state::{AppState, Session};
use crate::typography::{TextRole, TypographyExt as _};

/// The window title shown by the OS (Window menu, Mission Control, task bar).
pub const WINDOW_TITLE: &str = "Openmail";

/// Size of a newly opened main window.
pub const DEFAULT_SIZE: gpui::Size<Pixels> = size(px(1200.), px(780.));

/// Smallest size the main window can be resized to while all three panes fit.
pub const MIN_SIZE: gpui::Size<Pixels> = size(px(760.), px(480.));

/// Width of the label sidebar.
pub const SIDEBAR_WIDTH: Pixels = px(220.);

/// Width of the thread list.
pub const THREAD_LIST_WIDTH: Pixels = px(360.);

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

/// The root view of the main window.
pub struct MainWindow {
    model: Entity<AppModel>,
    sidebar: Entity<SidebarView>,
    conversation: Entity<ConversationView>,
    _subscriptions: [Subscription; 2],
}

impl MainWindow {
    /// Creates the view. It re-renders when the session or sync status
    /// changes, and keeps the theme in step with the system appearance.
    pub fn new(window: &mut Window, cx: &mut Context<'_, Self>) -> Self {
        let model = AppModel::global(cx);
        let appearance = crate::theme::follow_system_appearance(window, cx);
        let status = cx.observe(&model, |_, _, cx| cx.notify());
        Self {
            model,
            sidebar: cx.new(SidebarView::new),
            conversation: cx.new(ConversationView::new),
            _subscriptions: [appearance, status],
        }
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

    fn mailbox(
        sidebar: Entity<SidebarView>,
        conversation: &Entity<ConversationView>,
        cx: &App,
    ) -> impl IntoElement {
        let theme = cx.theme();
        h_flex()
            .flex_1()
            .min_h_0()
            // Sidebar (PR 3.3).
            .child(
                div()
                    .h_full()
                    .w(SIDEBAR_WIDTH)
                    .flex_none()
                    .bg(theme.sidebar)
                    .border_r_1()
                    .border_color(theme.sidebar_border)
                    .child(sidebar),
            )
            // Thread list (PR 3.4).
            .child(
                div()
                    .h_full()
                    .w(THREAD_LIST_WIDTH)
                    .flex_none()
                    .bg(theme.colors.list)
                    .border_r_1()
                    .border_color(theme.border),
            )
            // Conversation view (PR 3.5).
            .child(
                div()
                    .h_full()
                    .flex_1()
                    .min_w_0()
                    .child(conversation.clone()),
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

impl Render for MainWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let state = self.model.read(cx).state();
        let theme = cx.theme();
        let content = match Content::for_state(state) {
            Content::Mailbox => {
                Self::mailbox(self.sidebar.clone(), &self.conversation, cx).into_any_element()
            }
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
    fn minimum_width_fits_fixed_panes() {
        assert!(MIN_SIZE.width > SIDEBAR_WIDTH + THREAD_LIST_WIDTH);
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
