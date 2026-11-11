//! Letting the reading pane's webview show its own cursor: a pointing hand
//! over links, an I-beam over text.
//!
//! On macOS, WebKit sets the cursor the page asks for only while no AppKit
//! cursor rect is in force (`PageClientImpl::setCursor` returns early while
//! `[NSApp _cursorRectCursor]` is set). GPUI covers its whole view with one
//! cursor rect in its current style (`resetCursorRects`), and the webview
//! is a subview of it, so WebKit never got to set a cursor and the arrow
//! stayed. While the pointer is over the visible webview, GPUI's view drops
//! its cursor rects, again on every move in case AppKit or GPUI rebuilt
//! them; once the pointer leaves (or the webview hides or goes away) they
//! are rebuilt, and GPUI's cursor comes back.
//!
//! Elsewhere this does nothing: WebView2 and WebKitGTK set their own cursor
//! in their child windows.

use gpui::{AnyElement, IntoElement as _};

/// The webview's hold on the cursor, for one [`MessageBodyView`].
///
/// [`MessageBodyView`]: crate::message_body::MessageBodyView
#[derive(Default)]
pub struct WebviewCursor {
    #[cfg(target_os = "macos")]
    held: std::rc::Rc<std::cell::RefCell<Option<mac::CursorRectsDropped>>>,
}

impl WebviewCursor {
    /// An element to lay over the webview: it watches the pointer and gives
    /// the cursor to the webview while it is over it, if `visible`.
    #[cfg(target_os = "macos")]
    pub fn tracker(&self, visible: bool) -> AnyElement {
        use gpui::{DispatchPhase, MouseExitEvent, MouseMoveEvent, Styled as _, canvas};

        if !visible {
            self.release();
        }
        let held = self.held.clone();
        canvas(
            |_, _, _| {},
            move |bounds, (), window, _| {
                let on_move = held.clone();
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, _| {
                    if phase != DispatchPhase::Capture {
                        return;
                    }
                    let mut held = on_move.borrow_mut();
                    if !visible || !bounds.contains(&event.position) {
                        held.take();
                    } else if let Some(held) = held.as_ref() {
                        held.drop_again();
                    } else {
                        *held = mac::CursorRectsDropped::new(window);
                    }
                });
                let on_exit = held.clone();
                window.on_mouse_event(move |_: &MouseExitEvent, _, _, _| {
                    on_exit.borrow_mut().take();
                });
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .into_any_element()
    }

    /// Nothing to lay over the webview off macOS.
    #[cfg(not(target_os = "macos"))]
    pub fn tracker(&self, _visible: bool) -> AnyElement {
        gpui::Empty.into_any_element()
    }

    /// Gives the cursor back to GPUI (the webview hid or is going away).
    pub fn release(&self) {
        #[cfg(target_os = "macos")]
        self.held.borrow_mut().take();
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use objc2::Message as _;
    use objc2::rc::Retained;
    use objc2_app_kit::NSView;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    /// GPUI's view without its cursor rects, until this is dropped.
    pub struct CursorRectsDropped {
        view: Retained<NSView>,
    }

    impl CursorRectsDropped {
        /// Drops the cursor rects of GPUI's view in `window`, if it is an
        /// AppKit window.
        pub fn new(window: &gpui::Window) -> Option<Self> {
            let dropped = Self {
                view: gpui_view(window)?,
            };
            dropped.drop_again();
            Some(dropped)
        }

        /// Drops them again, in case they were rebuilt.
        pub fn drop_again(&self) {
            self.view.discardCursorRects();
        }
    }

    impl Drop for CursorRectsDropped {
        fn drop(&mut self) {
            // AppKit asks GPUI's view for its cursor rect again.
            if let Some(window) = self.view.window() {
                window.invalidateCursorRectsForView(&self.view);
            }
        }
    }

    /// GPUI's `NSView` for `window`.
    // The crate's only unsafe code: a raw window handle is a bare pointer.
    #[allow(unsafe_code)]
    fn gpui_view(window: &gpui::Window) -> Option<Retained<NSView>> {
        let handle = HasWindowHandle::window_handle(window).ok()?;
        let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
            return None;
        };
        // SAFETY: an AppKit window handle's `ns_view` points to the window's
        // live `NSView` (GPUI's), and this runs on the main thread, in a GPUI
        // mouse handler, while the window exists. The view is retained
        // before the borrow ends.
        let view: &NSView = unsafe { handle.ns_view.cast::<NSView>().as_ref() };
        Some(view.retain())
    }
}
