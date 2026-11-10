//! GPUI views, theme, keymap and the bridge between `MailService` events and UI state.

mod main_window;
mod message_body;

pub use main_window::{MainWindow, main_window_options};
pub use message_body::{
    BODY_CSP, MessageBodyView, SAMPLE_NEWSLETTER, allows_navigation, body_document,
};

use gpui::{App, AppContext as _, Bounds, WindowHandle};
use gpui_component::{Root, Theme};

/// Initializes `gpui-component` and the app theme. Call once, before opening windows.
pub fn init(cx: &mut App) {
    gpui_component::init(cx);
    Theme::sync_system_appearance(None, cx);

    // There is no menu bar or dock handling yet, so quit with the last window.
    cx.on_window_closed(|cx, _| {
        if cx.windows().is_empty() {
            cx.quit();
        }
    })
    .detach();
}

/// Opens the main Openmail window, centered on the primary display.
pub fn open_main_window(cx: &mut App) -> gpui::Result<WindowHandle<Root>> {
    let bounds = Bounds::centered(None, main_window::DEFAULT_SIZE, cx);
    cx.open_window(main_window_options(bounds), |window, cx| {
        let view = cx.new(|cx| MainWindow::new(window, cx));
        cx.new(|cx| Root::new(view, window, cx))
    })
}
