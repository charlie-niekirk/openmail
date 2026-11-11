//! The reading pane's webview: one native child webview (`wry`, through
//! `gpui-webview`) per window, placed at a GPUI layout rect, showing the open
//! conversation's document (see [`body_document`](crate::body_document)).
//!
//! The webview is created the first time there is something to show, then
//! reused: a new conversation is loaded into it in place. Until a thread's
//! document is ready the old one stays up briefly ([`MessageBodyView::hold`]),
//! so switching threads doesn't flash. It is hidden while a GPUI dialog or
//! sheet is open (the native view would cover it) and dropped on sign-out.
//!
//! JavaScript is off, storage is incognito, and the webview may only navigate
//! to a document this view is loading. See "Webview embedding" in
//! `docs/ARCHITECTURE.md` for the platform findings.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render, SharedString,
    Styled as _, Task, Window, div,
};
use gpui_component::WindowExt as _;
use gpui_webview::WebView;
use tokio::sync::mpsc;
use wry::{NewWindowResponse, PageLoadEvent, WebViewBuilder};

/// How long the previous document stays up while the next one is prepared.
const HOLD: Duration = Duration::from_millis(200);

/// How long a hidden webview waits for its new document to finish loading
/// before it is shown anyway.
const REVEAL_TIMEOUT: Duration = Duration::from_millis(400);

/// Whether the webview may navigate to `url`. It may load a document this
/// view is loading (`loading`): `load_html` loads at `about:blank` on WebKit
/// and as a `data:text/html` URL on WebView2. It may also follow links to
/// anchors within the document. Everything else, link clicks included, is
/// cancelled; PR 4.2 opens links in the system browser.
pub fn allows_navigation(url: &str, loading: bool) -> bool {
    let document = url == "about:blank" || url.starts_with("data:text/html");
    (document && loading) || url.starts_with("about:blank#")
}

/// The webview, once there has been something to show.
enum Web {
    NotCreated,
    Ready(Entity<WebView>),
    /// The platform can't host a child webview here (for example a native
    /// Wayland window on Linux); the reading pane falls back to text.
    Unavailable(SharedString),
}

/// The reading pane's webview and the document it shows.
pub struct MessageBodyView {
    web: Web,
    /// The document to show, if any.
    document: Option<String>,
    /// Whether `document` has yet to be loaded into the webview.
    stale: bool,
    /// Set from loading a document until it finishes. Shared with the
    /// navigation handler: the only time the webview may navigate.
    loading: Rc<Cell<bool>>,
    /// Whether the webview stays hidden until its new document has loaded,
    /// so it never shows the previous one or a blank page first.
    revealing: bool,
    /// Shows the webview if its document takes too long to report loading.
    reveal_timeout: Option<Task<()>>,
    /// Hides the webview once the hold runs out.
    hold: Option<Task<()>>,
    /// Tells this view when a page has finished loading.
    page_loads: mpsc::UnboundedSender<()>,
    _page_loads: Task<()>,
    /// WebView2's user data folder, kept alive with the webview.
    #[cfg(windows)]
    _context: Option<wry::WebContext>,
}

impl MessageBodyView {
    pub fn new(cx: &mut Context<'_, Self>) -> Self {
        let (page_loads, mut loaded) = mpsc::unbounded_channel();
        let listen = cx.spawn(async move |this, cx| {
            while loaded.recv().await.is_some() {
                if this.update(cx, Self::page_loaded).is_err() {
                    break;
                }
            }
        });
        Self {
            web: Web::NotCreated,
            document: None,
            stale: false,
            loading: Rc::new(Cell::new(false)),
            revealing: false,
            reveal_timeout: None,
            hold: None,
            page_loads,
            _page_loads: listen,
            #[cfg(windows)]
            _context: None,
        }
    }

    /// Shows `document`, a complete document from
    /// [`conversation_document`](crate::body_document::conversation_document).
    /// It is loaded into the webview on the next frame unless it is already
    /// the one shown.
    pub fn show(&mut self, document: String, cx: &mut Context<'_, Self>) {
        self.hold = None;
        if self.document.as_ref() != Some(&document) {
            self.document = Some(document);
            self.stale = true;
        }
        cx.notify();
    }

    /// Keeps the current document up while the next is prepared (a thread
    /// opening, or its bodies loading), then hides it after [`HOLD`] unless
    /// [`MessageBodyView::show`] comes first.
    pub fn hold(&mut self, cx: &mut Context<'_, Self>) {
        if self.document.is_none() || self.hold.is_some() {
            return;
        }
        self.hold = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(HOLD).await;
            this.update(cx, Self::clear).ok();
        }));
    }

    /// Shows nothing, hiding the webview now.
    pub fn clear(&mut self, cx: &mut Context<'_, Self>) {
        self.hold = None;
        self.document = None;
        self.stale = false;
        if let Web::Ready(webview) = &self.web {
            let _ = webview.read(cx).set_visible(false);
        }
        cx.notify();
    }

    /// Drops the webview (as on sign-out). The next document creates a new
    /// one.
    pub fn tear_down(&mut self, cx: &mut Context<'_, Self>) {
        self.clear(cx);
        // Dropping the entity hides the native view and hands focus back to
        // the window; it is destroyed once its last frame lets go of it.
        self.web = Web::NotCreated;
        self.revealing = false;
        self.reveal_timeout = None;
        self.loading.set(false);
        #[cfg(windows)]
        {
            self._context = None;
        }
    }

    /// Whether there is a document to show (or one held while the next is
    /// prepared).
    pub fn has_document(&self) -> bool {
        self.document.is_some()
    }

    /// Why the webview can't be shown here, if it can't.
    pub fn unavailable(&self) -> Option<&SharedString> {
        match &self.web {
            Web::Unavailable(reason) => Some(reason),
            _ => None,
        }
    }

    /// Creates the webview if there is a document to show and none exists
    /// yet. Call before rendering this view.
    pub fn prepare(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.document.is_none() || !matches!(self.web, Web::NotCreated) {
            return;
        }

        // WebView2 needs a writable user data folder even when incognito;
        // by default it would be made next to the executable.
        #[cfg(windows)]
        let mut context = webview_data_dir().map(|dir| wry::WebContext::new(Some(dir)));
        #[cfg(windows)]
        let builder = match context.as_mut() {
            Some(context) => WebViewBuilder::new_with_web_context(context),
            None => WebViewBuilder::new(),
        };
        #[cfg(not(windows))]
        let builder = WebViewBuilder::new();

        let builder = harden(builder, self.loading.clone(), self.page_loads.clone());
        let built = WebView::build(builder, window, cx);
        #[cfg(windows)]
        {
            self._context = context;
        }
        self.web = match built {
            Ok(webview) => {
                // Hidden until its first document has loaded (see `render`).
                let _ = webview.set_visible(false);
                self.stale = true;
                Web::Ready(cx.new(|_| webview))
            }
            Err(err) => {
                tracing::warn!("message body webview unavailable: {err}");
                Web::Unavailable(format!("HTML mail can't be shown here: {err}").into())
            }
        };
    }

    /// The page this view loaded has finished loading.
    fn page_loaded(&mut self, cx: &mut Context<'_, Self>) {
        if self.revealing {
            self.revealing = false;
            self.reveal_timeout = None;
            cx.notify();
        }
    }

    /// Keeps the webview hidden until the document being loaded is ready.
    fn reveal_when_loaded(&mut self, cx: &mut Context<'_, Self>) {
        self.revealing = true;
        self.reveal_timeout = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(REVEAL_TIMEOUT).await;
            this.update(cx, Self::page_loaded).ok();
        }));
    }
}

/// The spike's hardening: JavaScript off, incognito, no devtools, autoplay,
/// clipboard, zoom keys, swipe navigation or new windows, and navigation
/// only to a document being loaded (`loading`). `page_loads` hears each page
/// finish loading, which also clears `loading`.
fn harden<'a>(
    builder: WebViewBuilder<'a>,
    loading: Rc<Cell<bool>>,
    page_loads: mpsc::UnboundedSender<()>,
) -> WebViewBuilder<'a> {
    let loaded = loading.clone();
    builder
        .with_javascript_disabled()
        .with_incognito(true)
        .with_devtools(false)
        .with_autoplay(false)
        .with_clipboard(false)
        .with_hotkeys_zoom(false)
        .with_back_forward_navigation_gestures(false)
        .with_navigation_handler(move |url| allows_navigation(&url, loading.get()))
        .with_new_window_req_handler(|_, _| NewWindowResponse::Deny)
        .with_on_page_load_handler(move |event, _| {
            if matches!(event, PageLoadEvent::Finished) {
                loaded.set(false);
                let _ = page_loads.send(());
            }
        })
}

/// Where WebView2 keeps its (otherwise unused) profile: the app's data
/// directory.
#[cfg(windows)]
fn webview_data_dir() -> Option<std::path::PathBuf> {
    match openmail_store::DataDir::from_env_or_platform() {
        Ok(dir) => Some(dir.root().join("webview")),
        Err(err) => {
            tracing::warn!("no data directory for the webview: {err}");
            None
        }
    }
}

impl Render for MessageBodyView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let Web::Ready(webview) = &self.web else {
            return div().size_full();
        };
        let webview = webview.clone();

        if self.stale
            && let Some(document) = &self.document
        {
            self.stale = false;
            self.loading.set(true);
            if let Err(err) = webview.read(cx).load_html(document) {
                tracing::warn!("could not load the conversation into the webview: {err}");
            }
            if !webview.read(cx).visible() {
                self.reveal_when_loaded(cx);
            }
        }

        // The native view would cover a dialog or sheet, so it steps aside.
        let covered = window.has_active_dialog(cx) || window.has_active_sheet(cx);
        let visible = self.document.is_some() && !self.revealing && !covered;
        let native = webview.read(cx);
        if native.visible() != visible {
            let _ = native.set_visible(visible);
        }
        div().size_full().child(webview)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_document_being_loaded_may_load() {
        assert!(allows_navigation("about:blank", true));
        assert!(allows_navigation(
            "data:text/html;charset=utf-8,<p>x</p>",
            true
        ));
        assert!(!allows_navigation("about:blank", false));
        assert!(!allows_navigation("data:text/html,<p>phish</p>", false));
    }

    #[test]
    fn anchors_within_the_document_are_followed() {
        assert!(allows_navigation("about:blank#top", false));
        assert!(allows_navigation("about:blank#top", true));
    }

    #[test]
    fn everything_else_is_cancelled() {
        for loading in [false, true] {
            assert!(!allows_navigation(
                "https://example.com/unsubscribe",
                loading
            ));
            assert!(!allows_navigation("http://example.com", loading));
            assert!(!allows_navigation("file:///etc/passwd", loading));
            assert!(!allows_navigation("javascript:alert(1)", loading));
            assert!(!allows_navigation("data:image/svg+xml,<svg/>", loading));
            assert!(!allows_navigation("about:srcdoc", loading));
        }
    }
}
