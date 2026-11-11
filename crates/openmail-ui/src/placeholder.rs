//! What a pane shows in place of its content (loading, empty, failed), and
//! the strip it shows above its content (offline, a failure). Shared so the
//! three panes' states look alike.

use gpui::{App, Div, ParentElement as _, SharedString, Styled as _, div, px};
use gpui_component::spinner::Spinner;
use gpui_component::{ActiveTheme as _, h_flex, v_flex};

use crate::typography::{TextRole, TypographyExt as _};

/// Widest a placeholder's text runs.
const TEXT_WIDTH: gpui::Pixels = px(320.);

/// A spinner in the middle of the pane.
pub(crate) fn loading() -> Div {
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .child(Spinner::new())
}

/// A title, with more detail below it, in the middle of the pane. Callers
/// can add a button.
pub(crate) fn placeholder(
    title: impl Into<SharedString>,
    detail: Option<SharedString>,
    cx: &App,
) -> Div {
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap_2()
        .px_6()
        .text_center()
        .child(
            div()
                .max_w(TEXT_WIDTH)
                .text_role(TextRole::Body)
                .text_color(cx.theme().muted_foreground)
                .child(title.into()),
        )
        .children(detail.map(|detail| {
            div()
                .max_w(TEXT_WIDTH)
                .text_role(TextRole::Secondary)
                .text_color(cx.theme().muted_foreground)
                .child(detail)
        }))
}

/// A strip across the top of a pane. Callers can add a button.
pub(crate) fn banner(text: impl Into<SharedString>, cx: &App) -> Div {
    let theme = cx.theme();
    h_flex()
        .flex_none()
        .w_full()
        .min_h(px(32.))
        .px_3()
        .py_1()
        .gap_2()
        .bg(theme.muted)
        .border_b_1()
        .border_color(theme.border)
        .text_role(TextRole::Secondary)
        .text_color(theme.muted_foreground)
        .child(div().flex_1().min_w_0().child(text.into()))
}
