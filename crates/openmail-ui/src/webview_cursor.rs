//! Letting the reading pane's webview show its own cursor: a pointing hand
//! over links, an I-beam over text.
//!
//! On macOS, GPUI covers its whole view with one AppKit cursor rect in GPUI's
//! current cursor style (`resetCursorRects`), and AppKit applies it again as
//! the mouse moves, also over the webview, a subview whose cursor WebKit sets
//! directly. So while the pointer is over the visible webview, the window's
//! cursor rects are turned off; once it leaves (or the webview hides or goes
//! away) they are turned back on and GPUI's cursor comes back.
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
    taken: std::rc::Rc<std::cell::RefCell<Option<mac::CursorRectsOff>>>,
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
        let taken = self.taken.clone();
        canvas(
            |_, _, _| {},
            move |bounds, (), window, _| {
                let on_move = taken.clone();
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, _| {
                    if phase != DispatchPhase::Capture {
                        return;
                    }
                    let over = visible && bounds.contains(&event.position);
                    let mut taken = on_move.borrow_mut();
                    if over && taken.is_none() {
                        *taken = mac::CursorRectsOff::new(window);
                    } else if !over {
                        taken.take();
                    }
                });
                let on_exit = taken.clone();
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
        self.taken.borrow_mut().take();
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use objc2::Message as _;
    use objc2::rc::Retained;
    use objc2_app_kit::{NSView, NSWindow};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    /// A window whose cursor rects are off until this is dropped.
    pub struct CursorRectsOff {
        window: Retained<NSWindow>,
        /// GPUI's view, whose cursor rect comes back.
        view: Retained<NSView>,
    }

    impl CursorRectsOff {
        /// Turns `window`'s cursor rects off, if it is an AppKit window.
        pub fn new(window: &gpui::Window) -> Option<Self> {
            let view = gpui_view(window)?;
            let window = view.window()?;
            window.disableCursorRects();
            Some(Self { window, view })
        }
    }

    impl Drop for CursorRectsOff {
        fn drop(&mut self) {
            self.window.enableCursorRects();
            self.window.invalidateCursorRectsForView(&self.view);
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
