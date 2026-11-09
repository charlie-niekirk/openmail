//! The main window: title bar plus a placeholder three-pane layout
//! (sidebar, thread list, reading pane).

use gpui::{
    Bounds, Context, IntoElement, ParentElement as _, Pixels, Render, Styled as _, Subscription,
    Window, WindowBounds, WindowOptions, div, px, size,
};
use gpui_component::{ActiveTheme as _, Theme, TitleBar, h_flex, v_flex};

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

/// The root view of the main window.
pub struct MainWindow {
    _appearance: Subscription,
}

impl MainWindow {
    /// Creates the view and keeps the theme in step with the system light/dark setting.
    pub fn new(window: &mut Window, cx: &mut Context<'_, Self>) -> Self {
        Theme::sync_system_appearance(Some(window), cx);
        let appearance = cx.observe_window_appearance(window, |_, window, cx| {
            Theme::sync_system_appearance(Some(window), cx);
        });
        Self {
            _appearance: appearance,
        }
    }
}

impl Render for MainWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = cx.theme();

        v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(TitleBar::new())
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .h_full()
                            .w(SIDEBAR_WIDTH)
                            .flex_none()
                            .bg(theme.sidebar)
                            .border_r_1()
                            .border_color(theme.sidebar_border),
                    )
                    .child(
                        div()
                            .h_full()
                            .w(THREAD_LIST_WIDTH)
                            .flex_none()
                            .bg(theme.colors.list)
                            .border_r_1()
                            .border_color(theme.border),
                    )
                    .child(div().h_full().flex_1().min_w_0().bg(theme.background)),
            )
    }
}

#[cfg(test)]
mod tests {
    use gpui::{WindowBounds, point};

    use super::*;

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
}
