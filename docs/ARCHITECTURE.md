# Architecture

This document describes how Openmail is put together and why. The work that builds
it is tracked in [ROADMAP.md](ROADMAP.md); day-to-day conventions are in
[CLAUDE.md](../CLAUDE.md). Update this file when a PR changes a decision recorded here.

## Repository layout

```
openmail/
├─ Cargo.toml                 # workspace: shared metadata, dependencies and lints
├─ CLAUDE.md                  # conventions every session reads
├─ docs/
│  ├─ ROADMAP.md              # phases and PRs, with checkboxes
│  └─ ARCHITECTURE.md         # this file
├─ crates/
│  ├─ openmail-core/
│  ├─ openmail-gmail/
│  ├─ openmail-mime/
│  ├─ openmail-store/
│  ├─ openmail-sync/
│  ├─ openmail-ui/
│  └─ openmail-cli/
└─ apps/openmail/             # the desktop binary
```

## Crate responsibilities

```
                 openmail-core
               ▲      ▲       ▲
   openmail-gmail  openmail-mime  openmail-store
               ▲      ▲       ▲
                 openmail-sync  ◄──── openmail-cli
                      ▲
                 openmail-ui
                      ▲
                 apps/openmail
```

- **`openmail-core`** — Domain types shared by everything: IDs, `Account`, `Label`
  (system or user, colors, visibility), `Thread`, `Message`, `Address`, `Attachment`,
  `Draft`, and error types. Pure data, no I/O.
- **`openmail-gmail`** — Everything that talks to Google. OAuth2 installed-app flow
  (PKCE, loopback redirect, refresh), a typed REST client over `reqwest` (rustls),
  `multipart/mixed` batch requests, pagination, and backoff on 429/5xx. Tested
  against `wiremock`, never the real API.
- **`openmail-mime`** — RFC 5322 / MIME. Parses raw messages (`mail-parser`) into
  text, HTML, inline `cid:` parts and attachments; builds outgoing mail
  (`mail-builder`) and encodes it as base64url. Tested against a `.eml` corpus.
- **`openmail-store`** — The local cache: SQLite via bundled `rusqlite` with FTS5,
  WAL mode, `rusqlite_migration` migrations, one data directory per account
  (`directories`). Owns all SQL and exposes typed queries (keyset-paginated thread
  lists, unread counts, thread detail).
- **`openmail-sync`** — The sync engine, the offline `pending_ops` queue, and
  `MailService`, the async facade the UI and CLI use. Owns the tokio runtime.
- **`openmail-ui`** — GPUI views and their view-models, theme, keymap, and the
  bridge from `MailService` events to GPUI entities. Hosts the webview bridge for
  message bodies.
- **`openmail-cli`** — A developer tool (`login`, `labels`, `threads`, `show`,
  `sync`) for exercising the lower crates against a real account without the UI.
- **`apps/openmail`** — The desktop binary. Starts `MailService`, opens the GPUI
  window, and connects the two.

## Decisions

### UI: GPUI with `gpui-component`

GPUI is Zed's GPU-accelerated Rust UI framework; `gpui-component` (Longbridge)
provides the widget set (lists, inputs, popovers, resizable panes, themes). This
keeps the whole app in Rust with native performance and no browser runtime for the
chrome. Electron and Tauri were rejected for footprint and because only the message
body needs HTML rendering.

GPUI is pre-1.0, so `gpui` and `gpui-component` are pinned to exact, mutually
compatible versions and upgraded only in dedicated PRs. `gpui-component` is
published on crates.io against `gpui-pre`, a crates.io snapshot of Zed's `gpui`, so
the workspace depends on `gpui-pre` and `gpui-pre-platform` (the platform backends
that create the `Application`) under the names `gpui` and `gpui-platform`, at the
exact version `gpui-component` requires. UI logic lives in plain view-model types so
it can be tested without a window.

### Backend: Gmail REST API, not IMAP

The Gmail REST API exposes Gmail's real model: native labels and thread IDs,
`history.list` delta sync, batch requests, Gmail `q` search syntax, and the settings
APIs (filters, send-as aliases, vacation responder). IMAP only exposes part of this
through the X-GM extensions, and has no equivalent for settings. Scopes requested:
`gmail.modify` and `gmail.settings.basic`.

### Message bodies: sandboxed `wry` webview, sanitized with `ammonia`

GPUI cannot render arbitrary HTML, and email HTML needs a real engine. A `wry`
child webview (WKWebView on macOS, WebView2 on Windows, WebKitGTK on Linux) is
embedded at the body's layout rect and renders **only the message body** — never
app chrome.

- HTML is sanitized with an email-specific `ammonia` allowlist before it reaches the
  webview.
- A strict CSP is applied and email JavaScript is disabled. The only script is a
  trusted, app-injected one used for auto-height.
- Remote images are blocked by default, behind a per-message or per-sender
  "show images" choice. `cid:` references are rewritten to local content.
- Link clicks are intercepted and opened in the system browser, showing the real URL.

Spike 0.5 proved the embedding on macOS; the findings, the approach and the
per-platform constraints are under [Webview embedding](#webview-embedding).

### Storage: SQLite with FTS5

A local SQLite database (bundled `rusqlite`, so FTS5 is always available) caches
accounts, labels, threads, messages, message-label links, bodies, attachments,
drafts, sync state and pending operations. FTS5 powers offline search. WAL mode lets
the UI read while sync writes. Thread lists use keyset pagination; the target is
the first page of a label in under 5 ms with 100k threads.

### Secrets: OS keychain via `keyring`

OAuth refresh tokens are stored in the OS credential store (macOS Keychain, Windows
Credential Manager, Secret Service on Linux) via `keyring`, keyed by account email,
never on disk in plain text and never in the repo. Access tokens last an hour and are
kept only in memory, refreshed shortly before they expire. The Google OAuth client
ID (and the optional secret Google issues to desktop clients) comes from
`OPENMAIL_GOOGLE_CLIENT_ID` / `OPENMAIL_GOOGLE_CLIENT_SECRET` or user config, so
anyone can bring their own client while the app is unverified.

Sign-in uses Google's installed-app flow: PKCE (S256), the system browser, and a
one-shot loopback redirect server on `127.0.0.1:<random port>` that validates
`state`. The scopes add `openid email` so the ID token names the account.

## Webview embedding

Findings from spike 0.5. The code is `MessageBodyView` in
`crates/openmail-ui/src/message_body.rs`; PR 4.1 turns it into the production
component.

### Approach

`gpui-component` 0.9.1 has no webview feature any more: its webview moved into
**`gpui-webview`**, released in lockstep with it (0.9.1, Apache-2.0). It resolves
against the pinned `gpui-pre` 0.3.12 with no second copy of GPUI, so the spike uses
it rather than hand-rolling `build_as_child`. `gpui-webview` is built on
**`lb-wry`** 0.53.3, Longbridge's fork of `wry`, which it does not re-export; the
workspace depends on it as `wry` at the same exact version.

- `gpui_webview::WebView::build(builder, window, cx)` calls `build_as_child` on the
  GPUI window's raw window handle (plus the Linux setup below).
- `WebView` renders a GPUI element whose `prepaint` pushes the element's layout
  rect to the native view (`set_bounds`) on every frame, and inserts a
  mouse-blocking hitbox so GPUI content underneath does not get the clicks.
  Clicking GPUI content outside the rect calls `focus_parent`, which hands
  keyboard focus back to GPUI.
- `MessageBodyView` builds the webview with JavaScript disabled
  (`with_javascript_disabled`: `allowsContentJavaScript = NO` on WKWebView,
  `IsScriptEnabled = false` on WebView2, `enable-javascript = false` on
  WebKitGTK), incognito storage, no devtools, autoplay, clipboard access, zoom
  hotkeys or swipe navigation, new windows denied, and a navigation handler that
  allows only the initial document (`about:blank` on WebKit, a `data:text/html`
  URL on WebView2). The HTML is a sanitized fragment wrapped in a trusted shell
  with a CSP of `default-src 'none'; img-src data:; style-src 'unsafe-inline'; …`.
- If the webview cannot be created (a native Wayland window), the view shows a
  notice instead of failing.

### macOS (WKWebView): works

Tested on macOS 27.0 (Apple silicon) with `cargo run -p openmail`, driving the window
with synthetic input:

- **Placement.** The page renders exactly in the rect below a GPUI subject header in
  the reading pane, with GPUI drawing around it.
- **Resize.** Programmatic resizes and a live drag of the window corner keep the
  native view aligned with the GPUI layout: a capture taken mid-drag showed no gap
  or overhang. The page reflows to the new width.
- **Scroll.** Trackpad and wheel scrolling work natively inside the webview, with
  the system overlay scrollbar. Arrow keys and Page Down scroll once it has focus.
- **Focus.** On launch GPUI's view is first responder and keys do not reach the
  page. Clicking the page makes WKWebView first responder; clicking GPUI content
  hands focus back, and keys then no longer scroll the page. One quirk: after a
  (blocked) link click, Space did not scroll the page, though arrows and Page Down
  did. 4.1 should check keyboard handling properly.
- **Privacy.** Link clicks reach the navigation handler and are cancelled; nothing
  navigates away. JavaScript is off and the CSP blocks every network load.
- **Occlusion.** WebKit does not paint while the window is fully covered by other
  windows, so a capture of a hidden window shows an empty grey rect. It paints as
  soon as the window is visible again.
- No extra system dependencies: WebKit ships with macOS.

### Constraints on every platform (gpui-pre backend)

From `gpui-webview`'s documentation and code; the spike's layout does not exercise
them.

- **The native view sits above GPUI.** Inside its rect it covers GPUI popovers,
  menus, dialogs, tooltips and toasts.
- **It cannot be partially clipped.** In a scrolling container it keeps its full
  bounds and spills past the container's edges; it is hidden only once it is
  entirely out of view.
- `gpui-webview`'s `gpui-fast` feature fixes both on macOS and Linux through window
  composition, but it requires the GPUI Fast backend instead of `gpui-pre`. That is
  a GPUI upgrade decision for a dedicated PR, not something to switch on in 4.1.

What this means for 4.1: render the open conversation as **one webview with all
its messages in one document**, rather than one webview per message inside a GPUI
scroll list. Keep overlays (label picker, menus, toasts) out of the body rect, or
hide the webview while an overlay that overlaps it is open.

### Windows (WebView2): not tested

The spike compiles and passes tests on Windows in CI, but it has not been run on
Windows. Known constraints, from the WebView2 and wry documentation:

- The **WebView2 Runtime** must be installed. Windows 11 ships it; on Windows 10 the
  installer (PR 9.x) should bundle the Evergreen bootstrapper.
- WebView2 needs a writable **user data folder**, even with incognito (InPrivate).
  By default it is created next to the executable, which fails under
  `Program Files`. 4.1 should point it at the app's data directory through a wry
  `WebContext` data directory.
- `with_html` uses `NavigateToString`, which is limited to **2 MB** of HTML. Large
  messages need a custom protocol (`with_custom_protocol`) that serves the body.
- The webview is a child HWND, so the overlay limits above apply, and `gpui-fast`
  composition does not cover Windows.
- The navigation handler's allowance for the initial `data:text/html` URL is
  untested on WebView2.

### Linux (WebKitGTK): not tested

The spike compiles and passes tests on Linux in CI (with `libwebkit2gtk-4.1-dev`
and `libgtk-3-dev`), but it has not been run on Linux.

- `gpui-webview` hosts WebKitGTK in an X11 child of the GPUI window. It
  initializes GTK on the X11 backend and pumps the GTK main loop from GPUI's
  executor every 16 ms. It also forwards page clicks to GPUI for focus and matches
  the page zoom to GPUI's scale factor, since GTK only scales by whole numbers.
- **X11 works** for child webviews, including XWayland.
- **Wayland is the risk, and it fails.** GTK cannot embed into a Wayland surface it
  does not own, so `WebView::build` returns an error on a native Wayland window.
  `gpui_platform::application()` picks Wayland whenever a compositor is available,
  so on most current desktops the spike shows its "can't be displayed" notice.
- Runtime packages: `libwebkit2gtk-4.1-0` and GTK 3 (for packaging in 9.x).

### Fallback plan

1. **Linux Wayland:** start the app on X11 with
   `gpui_platform::linux(WindowingModes::X11)`, which runs under XWayland on
   Wayland desktops, and offer an opt-out for users who prefer native Wayland. The
   cost is XWayland's weaker fractional scaling. This is a one-line change in
   `apps/openmail`, left to 4.1 so it can be tested on real hardware.
2. **No child webview at all** (native Wayland opt-out, missing WebView2 Runtime,
   or a build failure): render text-first. Show the `text/plain` part, or text
   converted from the sanitized HTML, natively in GPUI, with an "Open in browser"
   action. `MessageBodyView` already has the failure branch where this goes.
3. **If `gpui-webview` or `lb-wry` stalls** (the fork currently trails upstream wry
   0.57): replace it with a small element of our own over upstream `wry` using the
   same pattern (`build_as_child`, then `set_bounds` in `prepaint`).
   `gpui-webview` is a few hundred lines under Apache-2.0, so it can serve as a
   reference.
4. **If overlays over the body become essential** before a GPUI Fast upgrade: hide
   the webview while such overlays are open, or show them in a separate window.

## Threading model

```
┌──────────────── main thread ────────────────┐     ┌──────── tokio runtime ────────┐
│ GPUI app + executor                          │     │ MailService                    │
│   views ── view-models ── event bridge ◄─────┼─────┤   sync engine                  │
│                    │                         │ evt │   pending_ops replay           │
│                    └── commands ─────────────┼────►│   openmail-gmail / -store      │
└──────────────────────────────────────────────┘ cmd └────────────────────────────────┘
```

- GPUI owns the main thread and has its own executor.
- All network, database and sync work runs in `MailService` on a dedicated tokio
  runtime.
- The UI sends commands (`Archive`, `LoadThread`, `Search`, …) over a channel.
  `MailService` emits events (`ThreadsChanged`, `SyncStatus`, …) on another.
- `openmail-ui` receives events in a task started with `cx.spawn` and updates GPUI
  entities, which re-render.
- The UI never blocks on I/O.

## Sync model

1. **Initial sync.** Fetch labels, then thread metadata: Inbox first, then everything
   else back to a configurable date, using batched metadata requests. Progress and
   the starting `historyId` are checkpointed so the sync can resume. Bodies load
   lazily, with background prefetch of recent mail.
2. **Incremental sync.** Poll `history.list` from the stored `historyId` — about every
   30 s while the window is focused, less often when idle — and apply added/deleted
   messages and added/removed labels. If the `historyId` has expired (404), do a full
   resync. Online/offline state is tracked so polling pauses when offline. Gmail push
   needs Pub/Sub and a server, so desktop polling is the deliberate choice.
3. **Local changes.** User actions apply optimistically to SQLite, are written to the
   persisted `pending_ops` queue, and replay against the API with retries. After a
   history refresh, the server's version wins.
