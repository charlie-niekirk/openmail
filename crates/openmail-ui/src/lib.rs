//! GPUI views, theme, keymap and the bridge between `MailService` events and UI state.
//!
//! Start-up, in order: [`init`] (widgets and themes), [`AppModel::init`] (starts
//! the mail service off the main thread), then [`open_main_window`].
//!
//! UI state lives in plain view-models ([`AppState`], [`Content`]) that are
//! tested without a window; [`AppModel`] is the GPUI entity that feeds
//! [`AppState`] from the service's events and hands views a `MailHandle`.

mod assets;
mod bridge;
mod conversation;
mod conversation_view;
mod main_window;
mod message_body;
mod onboarding;
mod onboarding_view;
mod panes;
mod sidebar;
mod sidebar_view;
mod state;
mod theme;
mod thread_list;
mod thread_list_view;
mod time_format;
mod typography;

pub use assets::{Assets, IMPORTANCE_ICON};
pub use bridge::{AppEvent, AppModel, Backend, Connection};
pub use conversation::{BodyState, Conversation, MessageItem};
pub use conversation_view::ConversationView;
pub use main_window::{Content, MainWindow, main_window_options};
pub use message_body::{
    BODY_CSP, MessageBodyView, SAMPLE_NEWSLETTER, allows_navigation, body_document,
};
pub use onboarding::{Notice, Setup, SignIn, SignInError, inbox_ready};
pub use panes::{PaneCommand, on_sidebar, on_thread_list, thread_view};
pub use sidebar::{Counts, Group, Mailbox, MailboxRow, Row, SNOOZED, Sidebar, parse_hex_color};
pub use sidebar_view::{SidebarEvent, SidebarView};
pub use state::{AppState, Changed, Session, SyncState};
pub use theme::{Themes, follow_system_appearance};
pub use thread_list::{
    Finished, LabelChip, Query, QueryKind, ThreadList, ThreadRow, condense_senders, format_date,
    inbox,
};
pub use thread_list_view::{ROW_HEIGHT, ThreadListEvent, ThreadListView};
pub use typography::{TextRole, TypographyExt};

use gpui::{App, AppContext as _, Bounds, WindowHandle};
use gpui_component::Root;

/// Initializes `gpui-component` and the app theme. Call once, before opening windows.
pub fn init(cx: &mut App) {
    gpui_component::init(cx);
    theme::init(cx);

    // There is no menu bar or dock handling yet, so quit with the last window.
    cx.on_window_closed(|cx, _| {
        if cx.windows().is_empty() {
            cx.quit();
        }
    })
    .detach();
}

/// Opens the main Openmail window, centered on the primary display. Call after
/// [`AppModel::init`].
pub fn open_main_window(cx: &mut App) -> gpui::Result<WindowHandle<Root>> {
    let bounds = Bounds::centered(None, main_window::DEFAULT_SIZE, cx);
    cx.open_window(main_window_options(bounds), |window, cx| {
        let view = cx.new(|cx| MainWindow::new(window, cx));
        cx.new(|cx| Root::new(view, window, cx))
    })
}
