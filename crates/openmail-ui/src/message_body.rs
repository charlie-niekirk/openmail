//! The message body: sanitized email HTML in a native child webview (`wry`, through
//! `gpui-webview`) placed at a GPUI layout rect.
//!
//! This is the 0.5 spike. It shows static HTML with JavaScript disabled, a restrictive
//! CSP and all navigation blocked; PR 4.1 turns it into the production component. See
//! "Webview embedding" in `docs/ARCHITECTURE.md` for the platform findings.

use gpui::{
    AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render, SharedString,
    Styled as _, Window, div,
};
use gpui_component::{ActiveTheme as _, v_flex};
use gpui_webview::WebView;
use wry::{NewWindowResponse, WebViewBuilder};

use crate::body_document::body_document;

/// Whether the webview may navigate to `url`. Only the initial document load is
/// allowed: `with_html` loads at `about:blank` (WebKit) or as a `data:` URL
/// (WebView2). Link clicks are cancelled; PR 4.2 opens them in the system browser.
pub fn allows_navigation(url: &str) -> bool {
    url == "about:blank" || url.starts_with("about:blank#") || url.starts_with("data:text/html")
}

/// What the reading pane shows: the webview, or why it could not be created.
enum Body {
    Web(Entity<WebView>),
    Unavailable(SharedString),
}

/// Renders one message body. Owns its webview and keeps the native view's bounds in
/// step with this element's layout rect (via `gpui-webview`'s element, in prepaint).
pub struct MessageBodyView {
    body: Body,
}

impl MessageBodyView {
    /// Creates the view and its webview showing `body_html`, a sanitized fragment.
    ///
    /// If the platform cannot host a child webview (for example a native Wayland
    /// window on Linux), the view shows a notice instead of failing.
    pub fn new(body_html: &str, window: &mut Window, cx: &mut Context<'_, Self>) -> Self {
        let builder = WebViewBuilder::new()
            .with_html(body_document(body_html))
            .with_javascript_disabled()
            .with_incognito(true)
            .with_devtools(false)
            .with_autoplay(false)
            .with_clipboard(false)
            .with_hotkeys_zoom(false)
            .with_back_forward_navigation_gestures(false)
            .with_navigation_handler(|url| allows_navigation(&url))
            .with_new_window_req_handler(|_, _| NewWindowResponse::Deny);

        let body = match WebView::build(builder, window, cx) {
            Ok(webview) => Body::Web(cx.new(|_| webview)),
            Err(err) => {
                eprintln!("openmail: message body webview unavailable: {err}");
                Body::Unavailable(format!("This message can't be displayed here: {err}").into())
            }
        };
        Self { body }
    }
}

impl Render for MessageBodyView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        match &self.body {
            Body::Web(webview) => div().size_full().child(webview.clone()),
            Body::Unavailable(reason) => div().size_full().child(
                v_flex()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .p_6()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(reason.clone()),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_initial_document_may_load() {
        assert!(allows_navigation("about:blank"));
        assert!(allows_navigation("about:blank#top"));
        assert!(allows_navigation("data:text/html;charset=utf-8,<p>x</p>"));
        assert!(!allows_navigation("https://example.com/unsubscribe"));
        assert!(!allows_navigation("http://example.com"));
        assert!(!allows_navigation("file:///etc/passwd"));
        assert!(!allows_navigation("javascript:alert(1)"));
        assert!(!allows_navigation("data:image/svg+xml,<svg/>"));
    }
}
