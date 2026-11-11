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
  `multipart/mixed` batch requests, pagination, a client-side quota limiter, and
  backoff on 429/5xx. Tested against `wiremock`, never the real API.
- **`openmail-mime`** — RFC 5322 / MIME. Parses raw messages (`mail-parser`) into
  text, HTML, inline `cid:` parts and attachments; builds outgoing mail
  (`mail-builder`) and encodes it as base64url. Tested against a `.eml` corpus.
- **`openmail-store`** — The local cache: SQLite via bundled `rusqlite` with FTS5,
  WAL mode, embedded SQL migrations, one data directory per account
  (`directories`). Owns all SQL and exposes typed queries (keyset-paginated thread
  lists, unread counts, thread detail).
- **`openmail-sync`** — The sync engine, the offline `pending_ops` queue, and
  `MailService`, the async facade the UI and CLI use. Owns the tokio runtime.
- **`openmail-ui`** — GPUI views and their view-models, theme, keymap, and the
  bridge from `MailService` events to GPUI entities. Hosts the webview bridge for
  message bodies.
- **`openmail-cli`** — A developer tool (`login`, `labels`, `threads`, `show`,
  `sync`, `watch`, …) for exercising the lower crates against a real account without the UI.
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

The schema is described under [Local store](#local-store).

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

Findings from spike 0.5, then how PR 4.1 built the production component on
them ([The conversation document](#the-conversation-document)). The code is
`MessageBodyView` in `crates/openmail-ui/src/message_body.rs` and the document
builder in `crates/openmail-ui/src/body_document.rs`.

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
  URL on WebView2; since 4.1, only while the view is loading one). The HTML is a
  sanitized fragment wrapped in a trusted shell with a CSP of
  `default-src 'none'; img-src data:; style-src 'unsafe-inline'; …`.
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
  hands focus back, and keys then no longer scroll the page. The spike saw Space
  stop scrolling after a (blocked) link click; with 4.1's document Space, Page
  Down and the arrows all scroll after a link click.
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

### The conversation document

PR 4.1 renders an open conversation as **one document in one webview**, as the
constraints above require.

- **GPUI keeps the subject header** (subject, label chips, message count); the
  webview fills the rest of the reading pane and scrolls natively.
- **Each message is a `<details>` section** in a trusted shell
  (`conversation_document`): the `<summary>` is its header (sender, then the
  recipients when open or the snippet when closed, and the date), followed by
  the From/To/Cc/date lines and the body. Opening and closing messages is the
  engine's own `<details>` behaviour, so it works with JavaScript off. Unread
  messages and the latest start open, as in 3.5. Shell classes start with
  `om-`; message HTML loses `class` and `id`, so neither side styles the other.
- **Every body loads when the thread opens**, not when a message is first
  expanded: toggles happen inside the webview, out of GPUI's sight. The mail
  service fetches requests that arrive together in one batch, and the document
  is shown once every body has loaded or failed (a spinner until then).
- **Message HTML goes through an interim `ammonia` allowlist** (`clean_html`):
  `ammonia`'s defaults plus inline `style`, presentational attributes
  (`align`, `bgcolor`, `width`, `cellpadding`, …), `<center>`/`<font>` and
  `data:` images. Re-serializing balances the fragment's tags, so it can't close
  its section and spill into the shell. Plain-text bodies are escaped into a
  `white-space: pre-wrap` block.
- **Lifecycle.** One webview per window, created on the first frame that has a
  document (`MessageBodyView::prepare`) and reused: a new thread is loaded into
  it with `load_html`, and an unchanged document isn't reloaded (so sync
  refreshes don't reset it). While the next thread's document is prepared the
  previous one stays up for up to 200 ms (`hold`), so switching threads
  doesn't flash; past that it is hidden. A hidden (or new) webview stays hidden
  until its new page reports finished loading (or 400 ms pass), so it never
  shows the previous page or a blank one first. An empty, missing or failed
  pane hides it at once, and a change of account (or signing out) drops it
  (`tear_down`); dropping hides the native view and returns focus to GPUI.
- **Resizing** needs nothing more than the spike: the element pushes its rect
  every frame, so the dividers and window resizes keep it aligned.
- **Overlays.** The webview hides while a `gpui-component` dialog or sheet is
  open. Later popovers, menus and toasts (5.x) must stay out of the reading
  pane's body rect or hide the webview the same way.
- **Focus.** Clicking the page gives it the keyboard (arrows, Space, Page Down
  scroll it); clicking GPUI content hands it back. A focused `<summary>`
  toggles on Space or Return, as in any browser.
- **No webview** (native Wayland, a build error): the reading pane draws the
  messages in GPUI as text, as 3.5 did, under a note saying why.

Left for later, without blocking it:

- **4.2** replaces `clean_html`'s allowlist. Email `<style>` elements are
  dropped for now; keeping them needs scoping to the message (selector
  prefixing, or a declarative shadow root per message). Remote images, `cid:`
  rewriting and opening links slot into `clean_html` and the navigation
  handler.
- **4.3.** One document grows with its content, so per-message auto-height is
  not needed. A trusted injected script could keep open sections and the
  scroll position across reloads (today a reload, as when a new message
  arrives, resets both), collapse quoted text, and adapt the shell to dark mode
  (its colours are in one CSS block, `SHELL_CSS`).
- **Large threads on Windows**: `NavigateToString` caps a document at 2 MB, so a
  long thread of newsletters needs a custom protocol to serve it.

### Windows (WebView2): not tested

The spike compiles and passes tests on Windows in CI, but it has not been run on
Windows. Known constraints, from the WebView2 and wry documentation:

- The **WebView2 Runtime** must be installed. Windows 11 ships it; on Windows 10 the
  installer (PR 9.x) should bundle the Evergreen bootstrapper.
- WebView2 needs a writable **user data folder**, even with incognito (InPrivate).
  By default it is created next to the executable, which fails under
  `Program Files`. Since 4.1 it is `webview/` in the app's data directory,
  through a wry `WebContext`.
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
   `apps/openmail`, still to be made and tested on real hardware.
2. **No child webview at all** (native Wayland opt-out, missing WebView2 Runtime,
   or a build failure): render text-first. Show the `text/plain` part, or text
   converted from the sanitized HTML, natively in GPUI, with an "Open in browser"
   action. Since 4.1 the reading pane does this (text, without the action yet)
   whenever the webview can't be created.
3. **If `gpui-webview` or `lb-wry` stalls** (the fork currently trails upstream wry
   0.57): replace it with a small element of our own over upstream `wry` using the
   same pattern (`build_as_child`, then `set_bounds` in `prepaint`).
   `gpui-webview` is a few hundred lines under Apache-2.0, so it can serve as a
   reference.
4. **If overlays over the body become essential** before a GPUI Fast upgrade: hide
   the webview while such overlays are open, or show them in a separate window.

## Local store

`openmail-store` keeps one SQLite database per account at
`<data dir>/accounts/<account id>/mail.db`. The data directory is the platform's
(`~/Library/Application Support/Openmail` on macOS, `~/.local/share/openmail` on
Linux, `%APPDATA%\Openmail\data` on Windows), or `OPENMAIL_DATA_DIR` if set; tests
pass their own directory. Every connection sets `journal_mode=WAL`,
`synchronous=NORMAL`, `foreign_keys=ON` and a 5 s busy timeout.

Migrations are the SQL files in `crates/openmail-store/migrations/`, embedded with
`include_str!` and applied in one transaction; `PRAGMA user_version` counts those
applied. They are append-only. (`rusqlite_migration` was the plan, but its latest
release pins `rusqlite` 0.40, whose bundled SQLite has RUSTSEC-2026-0459; the
in-crate runner does the same job in a few lines.)

Dates are Unix milliseconds, history IDs integers, address lists JSON arrays.

| Table | Holds |
| --- | --- |
| `accounts` | The account the database belongs to (id, email, display name). |
| `labels` | Gmail labels: type, colors, label-list and message-list visibility. |
| `threads` | Thread summaries: subject, snippet, last-message date, message count, senders, `history_id`, and `is_unread` / `is_starred` flags. |
| `messages` | Message metadata: headers (subject, from/to/cc/bcc/reply-to, Message-ID, In-Reply-To, References), internal date, size, `has_attachments`. |
| `message_labels` | Labels per message: the source of truth for labels. |
| `thread_labels` | Labels per thread (the union over its messages), with a per-label message count and a copy of the thread's last-message date. |
| `bodies` | Lazily fetched bodies: text, HTML, fetched-at. |
| `attachments` | Attachment metadata keyed by message and MIME part ID, with an optional local path. |
| `drafts` | Draft IDs, pointing at their message. |
| `sync_state` | One row: `history_id`, an opaque backfill cursor, backfill-complete, last full sync, and the learned Gmail quota budget with the quota it was learned under. |
| `sync_seen_threads` | Threads listed by the running initial sync or full resync, so it can delete stored threads that no longer exist when it finishes. |
| `pending_ops` | The offline ops queue: a never-reused ID (`AUTOINCREMENT`), op type, JSON payload (the op and what it changed locally), state (`queued`, `sending`, `sent`), attempts, next retry, created at, sent at, last error. |
| `messages_fts` | FTS5 index over subject, sender, recipients (to, cc, bcc) and body text. |

Triggers keep the derived data consistent, so writers only touch messages and
their labels:

- Inserting or deleting a `message_labels` row adjusts `thread_labels`
  (adding the row, counting, or removing it when no message has the label).
- `thread_labels` rows for `UNREAD` and `STARRED` set the thread's flags.
- A thread's new last-message date is copied to its `thread_labels` rows.
- Changes to a message's subject or addresses, or to its body text, rebuild its
  `messages_fts` row. The FTS table is contentless (`contentless_delete=1`), so the
  text is not stored twice; its rowid is the message's integer `pk`.

Label views page with keysets over the index
`thread_labels (label_id, last_message_date DESC, thread_id DESC)`, so a page is
an index range scan with no sort; `threads (last_message_date DESC, id DESC)`
serves views over every thread.

The UI's read queries (`queries.rs`):

- `Store::thread_page(view, after, limit)` returns a page of `ThreadSummary` rows
  and an opaque `ThreadCursor` for the next one. A `ThreadView` is one label, every
  one of several labels (inbox tabs such as `INBOX` + `CATEGORY_SOCIAL`), or
  All Mail (threads with a message outside Spam and Trash).
- `Store::label_counts()` returns total and unread thread counts for every label
  in one query.
- `Store::thread_detail(id)` returns a thread with its messages oldest first, their
  labels, and any bodies already fetched.

Tests check with `EXPLAIN QUERY PLAN` that these use indexes (and that pages and
thread detail never sort), and
`cargo bench -p openmail-store` measures them over a synthetic 100k-thread mailbox.

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

### `MailService`

`MailService::start(client, MailServiceConfig)` opens the account's database and
starts a thread that runs a multi-threaded tokio runtime (two workers) with
four tasks, each with its own SQLite connection (WAL lets them read while
another writes):

| Task | Does |
| --- | --- |
| local | Answers `MailHandle` calls one at a time: thread pages, label counts, labels, thread detail, the queued ops; enqueues and undoes ops. |
| sync | Runs the `Scheduler`: the initial sync until it is complete, then history polls. |
| ops | Runs the `OpsQueue` replay worker. |
| bodies | Fetches message bodies on request and prefetches recent ones. |

- **Commands.** `MailHandle` is a cheap, cloneable handle. Each call sends a
  command over an unbounded channel and awaits a `oneshot` answer, so its
  futures, like the event stream, can be awaited on any executor, GPUI's
  included. `poll_now` and `set_activity` go straight to the scheduler.
- **Events.** `MailEvents` yields `MailEvent`s: `SyncStatus` (`Initial`
  progress, `UpToDate` after each poll, `Retrying`, `Stopped`),
  `Connectivity`, `ThreadsChanged` and `LabelsChanged`, `BodyFetched`, `Op`
  (the ops queue's events) and `OpsStopped`. Store changes are reported as
  invalidations without detail: the UI queries again what it shows, which
  the store answers in milliseconds. They come from initial-sync progress
  (labels or threads stored), polls that changed something, ops enqueued,
  undone or rolled back, and label creations Gmail confirmed.
- **Bodies.** `MailHandle::body` answers from the store, or else fetches the
  message (`format=raw`, parsed by `openmail-mime`) and stores it. Requests
  that arrive together go in one batch, and several callers waiting for the
  same body share one fetch. After each poll the task prefetches the bodies
  of the newest Inbox messages that have none (50 by default), ten at a
  time, fetching any requested body before the next ten. A prefetch that
  fails stops quietly until the next poll; everything goes through the
  client's quota budget.
- **Failure.** The scheduler stopping (sign-in, daily quota, store) is
  `SyncStatus::Stopped`; the ops worker stopping is `OpsStopped`. Queries keep
  working. Restarting the service resumes from the store's checkpoints.
- **Shutdown.** Dropping the service, or `shutdown`, cancels every task and
  joins the thread; unsent ops stay queued. Calls on a handle then fail with
  `Error::ServiceStopped`, and the event stream ends.

### UI state and the bridge

`openmail-ui` keeps what the window shows about the account in `AppState`, a
plain view-model with no GPUI types: the session (`Starting`, `SignedOut` with
the sign-in screen's state, `Ready`, `Failed`), the sync state, connectivity,
whether sending stopped, and whether a new account is still setting up.
`AppState::apply_all` takes a batch of `MailEvent`s and returns a `Changed`
(status, threads, labels, bodies fetched), and `status_text` gives the title
bar's line. Tests drive it with events directly.

`AppModel` is the GPUI entity around it, registered as a global
(`AppModel::global(cx)`):

- **Start-up.** `AppModel::init(cx, backend)` takes the app's `Backend` and runs
  its `connect` on GPUI's background executor, since opening the store and
  reading the keychain block. It returns a `Connection`: `Started` (a
  `MailService`, its `MailEvents` and the account), `SignedOut` (no account,
  maybe with a reason), `Unconfigured` (no OAuth client, with setup help) or
  `Failed`.
- **Sign-in.** `AppModel::sign_in` runs `Backend::sign_in` on a thread of its
  own, since it waits for the user in the browser, and `cancel_sign_in` cancels
  it through a `CancellationToken`. Success is a `Connection` like start-up's.
- **Events.** A task started with `cx.spawn` awaits `MailEvents::recv` on GPUI's
  executor. For each event it also takes every event already waiting, applies
  the batch to `AppState`, then notifies observers if the status changed and
  emits `AppEvent::ThreadsChanged`, `LabelsChanged` or `BodyFetched(id)`. A
  busy initial sync therefore costs one re-render and one re-query per batch.
- **Queries.** Views subscribe to the `AppEvent`s they care about and query
  through `AppModel::mail()`, a cloned `MailHandle` awaited in a view's own
  `cx.spawn` task. Nothing on the main thread waits for the store or Gmail.
- **Shutdown.** On quit, `AppModel` shuts the service down on the background
  executor; unsent ops stay queued.

### Onboarding

`apps/openmail` opens `OPENMAIL_ACCOUNT`, or else the only account with a
database in the data directory. With none, or without a refresh token for it,
the window shows the sign-in screen (`onboarding::SignIn`): "Sign in with
Google", then a cancellable wait while the user finishes on Google's consent
page. Without `OPENMAIL_GOOGLE_CLIENT_ID` the screen explains how to create an
OAuth client instead of offering to sign in.

The app's sign-in runs the PKCE + loopback flow (PR 1.2) on a current-thread
tokio runtime of its own, raced against the cancellation token, and saves the
refresh token to the keychain. It then creates the account's store and account
row and starts its `MailService` with a fresh OAuth client, since the sign-in
client's pooled connections belong to the runtime that just ended. Declining in
the browser, cancelling and failures return to the sign-in screen with a
notice.

A newly signed-in account shows the setup screen (`onboarding::Setup`) with
its initial-sync progress until the inbox is stored (`inbox_ready`: the sync
reached `Prefetch`, `Backfill` or `Done`, or is up to date); older mail keeps
syncing behind the mailbox. Accounts opened at start-up go straight to the
mailbox.

When the sync stops with an error that needs a new sign-in
(`openmail_sync::Error::requires_sign_in`: a missing, revoked or expired
refresh token), `AppState` returns to the sign-in screen with a notice naming
the account and ignores that service's remaining events, and `AppModel` shuts
the service down. Signing in again starts a new one.

### Sidebar

The sidebar's view-model, `sidebar::Sidebar`, holds the labels, their thread
counts, the collapsed groups and the selected `Mailbox` (a label, or All Mail,
which is not one). `Sidebar::rows` flattens it into what the view draws, in
Gmail web's order:

- The system mailboxes: Inbox, Starred, Snoozed (only if the account lists
  `SNOOZED`), Important, Sent, Drafts, All Mail, Spam, Trash. They show the
  counts Gmail shows: unread threads for Inbox, Important and Spam, every draft
  for Drafts, nothing for the rest.
- "Categories", collapsed at first, with Social, Updates, Forums and
  Promotions (Primary is the rest of the Inbox and is not listed).
- "Labels": the user labels as a tree split on `/`, sorted by name ignoring
  case, with their Gmail color and unread count. A label nests under its
  nearest listed ancestor, or is a root named by its full path if there is
  none. Gmail's label-list visibility applies (hidden, or shown only while
  unread), except to the selected label. Any label with nested labels
  collapses; collapse state lasts while the app runs.

`SidebarView` queries `labels` and `label_counts` through the `MailHandle`
when the service starts and again on every `LabelsChanged` or
`ThreadsChanged` (both are quick store reads), and emits
`SidebarEvent::Selected(Mailbox)` when the selection changes, including the
fallback to the Inbox when the selected label is deleted. The Inbox is
selected at start.

### Conversation view

The reading pane is `ConversationView` (`conversation_view.rs`) around the
`Conversation` view-model (`conversation.rs`), which has no GPUI types and is
tested directly. `ConversationView::open_thread(ThreadId)` loads the thread
(`MailHandle::thread`) and the labels, then shows the subject, label chips
(Inbox, Spam and Trash, then user labels with their Gmail colors) and the
message count, and the messages oldest first, the way Gmail does:

- Unread messages and the latest one start expanded; the rest collapse to
  sender, snippet and date. Clicking a message toggles it.
- An expanded message shows the sender, a short "to me, Bob" line, the full
  From, To, Cc and date, and its body. Since 4.1 the messages are one HTML
  document in a webview ([The conversation document](#the-conversation-document)),
  so every body loads through `MailHandle::body` when the thread opens. Without
  a webview they are drawn in GPUI as text: the plain text, or text converted
  from the HTML when there is none (`openmail_mime::html_to_text`).
- `ThreadsChanged` and `LabelsChanged` re-query the open thread, keeping what
  the user expanded and the bodies already loaded; new messages get the
  defaults. `BodyFetched` retries a body whose fetch had failed.

Dates are in local time, with the same conversion as the thread list
(`time_format::local`): compact dates on collapsed and expanded messages
(`format_date`), and the full date among an expanded message's participants.

The pane's state is a `ReadingPane` (`Empty`, `Loading`, `Missing`,
`Failed` with a retry, or `Open(Conversation)`), also in the view-model. A
body that can't be fetched while offline says so, and the expanded messages'
unavailable bodies load again when Gmail is reachable once more.

### Theme, typography and window frame

The light and dark themes are a `gpui-component` theme set embedded from
`crates/openmail-ui/themes/openmail.json`; colors it does not name fall back to
`gpui-component`'s defaults. `theme::init` installs them and picks the one
matching the system appearance, and each window keeps following it with
`follow_system_appearance` (an `observe_window_appearance` subscription).

Text uses the system UI font. Views pick a `TextRole` (`Title`, `Heading`,
`Body`, `Secondary`, `Caption`) through `TypographyExt::text_role` rather than raw
sizes; `Body` is the themes' 14 px base size.

The main window has a transparent title bar with the macOS traffic lights
(`gpui-component`'s `TitleBar`), showing the account and the sync status. Below
it are the three panes (see [Layout](#layout)), or a notice when there is no
mailbox.

Icons are SVGs served by `openmail_ui::Assets`, which the app registers with
`with_assets`: `gpui-component`'s default set (from `gpui-kit-assets`), the
Lucide icons the app adds to it through `icon_assets!`, and the app's own
(`crates/openmail-ui/assets/icons`).

### Thread list

`ThreadList` is the thread list's view-model; `ThreadListView` renders it in a
`uniform_list` of fixed-height rows, so only the visible rows are built.

- **Paging.** Threads load a page (50) at a time through
  `MailHandle::thread_page`'s keyset pagination, newest first. When the last
  visible row is within 25 of the end of what is loaded, the next page loads.
  One query runs at a time.
- **Refresh.** On `ThreadsChanged` the list re-queries everything loaded so far
  as one page from the top, so rows change in place, threads that gained a
  message move up and threads that left the view go. Unless the list is at the
  very top, the row on top of the viewport is kept where it was. A refresh asked
  for while a query runs follows it.
- **Views.** It shows the Inbox until `set_label` (or `set_view` with any
  `ThreadView`) switches it, which clears the list and the selection; results
  for the previous view are dropped.
- **Rows.** Senders are condensed as Gmail does (a lone sender's full name;
  otherwise first names, the first and the last two past three, "me" for the
  account), with the message count. Dates are relative to now in the local time
  zone ("3:42 PM" today, "Nov 3" this year, "3/14/24" before). Label chips show
  user labels Gmail shows in the message list, plus Inbox outside the Inbox, but
  not the view's own label; at most three, then "+N". Unread threads are bold;
  stars, Gmail's importance marker and a paperclip show when they apply.
- **Selection.** Clicking a row selects it and emits
  `ThreadListEvent::Selected(ThreadId)`; `selected()` returns it.
- **States.** `ThreadList::content` says what the pane shows: a spinner until
  the first page arrives, the failure with a retry if it doesn't, the rows, or
  "No conversations in <label>" with a reason more may come (offline, or still
  syncing). `ThreadList::banner` adds a strip above the rows when offline, or
  when loading more or refreshing failed (with a retry).

### Layout

`MainWindow` lays the sidebar, the thread list and the reading pane side by
side and wires them together through `panes.rs`, plain functions from one
pane's event to commands for the others: `SidebarEvent::Selected(mailbox)`
shows that mailbox's `ThreadView` in the list and closes the open thread;
`ThreadListEvent::Selected(id)` opens the thread in the reading pane.

The sidebar and the thread list end in dividers (`gpui-component`'s
standalone `resize_handle`); dragging one resizes its pane. `PaneSizes`
(`pane_sizes.rs`) holds the widths: each pane has a range (sidebar 160–400 px,
list 280–640 px), and a drag leaves the reading pane at least 300 px. The
widths laid out come from `PaneSizes::fit`, which shrinks the list, then the
sidebar, towards their minimums when the window is too narrow, without
changing the saved preference. The sizes are saved to `layout.json` in the
data directory (they belong to the user, not an account, so not in a mail
database) 300 ms after a divider stops moving, and read once before the
window's first frame.

The sidebar shows the system mailboxes at once, then "Loading labels…" until
the labels arrive, or why they didn't with a retry (`Sidebar::labels_state`).

## Sync model

1. **Initial sync.** Fetch labels, then thread metadata: Inbox first, then everything
   else back to a configurable date, using batched metadata requests. Progress and
   the starting `historyId` are checkpointed so the sync can resume. Bodies load
   lazily, with background prefetch of recent mail (see
   [`MailService`](#mailservice)).

   `InitialSync` in `openmail-sync` records the profile's `historyId` before
   anything else, then syncs labels, Inbox threads, and a backfill of the rest,
   each newest-first down to a cutoff (90 days by default; Gmail's `after:`
   operator; older threads are left for on-demand fetching), in
   `threads.list` pages of 100. Each page's changed threads are fetched
   (`format=metadata` with From, To, Cc, Subject, Date and Message-ID) in
   batches of 5, three in flight, and written in one transaction together
   with a JSON checkpoint (phase, page token, cutoff) in
   `sync_state.backfill_cursor`, so a failed or cancelled sync resumes at the
   next page. Threads already stored with the same history ID are not fetched
   again. Optionally, the newest Inbox bodies are fetched (`format=raw`,
   parsed by `openmail-mime`) between the Inbox and the backfill.

   **Single-message threads.** Most threads have one message, and
   `messages.get` costs 5 quota units against `threads.get`'s 10, so those
   are fetched as messages. Listing stays with `threads.list`, because it
   gives each thread's history ID: the unchanged-thread skip, the resync
   bookkeeping and the per-page checkpoint work as before. To tell
   single-message threads apart without fetching them, the sync pages
   through `messages.list` with the same cutoff (500 IDs, 5 units, a page),
   only as far as the threads it is fetching, and keeps the first message
   it sees for each thread, which is the newest. Gmail gives a thread its
   first message's ID, so a thread whose newest listed message has the
   thread's ID has no other message. Every other thread is fetched whole
   with `threads.get`.

   That settles the cutoff edge without storing anything partially: a
   thread whose earlier messages predate `after:` has its first message
   outside the listing, so it is never taken for a single-message thread
   and is always fetched whole. Fetching those threads whole beats marking
   them partial and filling them in later: they are a small share, a
   `threads.get` costs no more than two `messages.get`, and the UI never sees
   a thread with the wrong count, senders or first subject. If a thread's
   first message has been deleted, or is in Spam or Trash, it is not
   listed either, and the thread is fetched whole. A single-message thread
   is stored with the history ID `threads.list` gave it, so later
   comparisons are like for like. Two assumptions carry this: thread IDs
   are first-message IDs, and `messages.list` is newest first. If either
   failed, a thread could be stored without some of its newer messages
   until the next change to it, which incremental sync refetches whole.
   `InitialSyncConfig::single_message_fetch` turns it off. For the 30-day
   test mailbox (about 920 threads, 955 messages, nearly all single) this
   cuts the cost from about 9,300 units to about 5,000.

   **Gmail quota.** Gmail limits each user to a number of quota units a
   minute, set per Cloud project as "Units per minute per user". Projects may
   get only **6,000** (it used to be 15,000), and since anyone can bring
   their own client, Openmail assumes 6,000 unless told otherwise
   (`GmailClientBuilder::quota_per_minute`, `OPENMAIL_GMAIL_QUOTA_PER_MINUTE`,
   or the CLI's `--quota-per-minute`). Calls cost from 1 unit (`labels.list`)
   to 10 (`threads.get`) or more. Failed calls appear to count too.

   Measured on a real account with a 6,000 quota, Gmail turned the client
   away once its trailing 60 s spend reached about 2,700–2,800 units by the
   documented costs: either `threads.get` is effectively charged about 20,
   or the effective limit is about half the quota. Per-second pacing did not
   help. An adaptive rate tripped Gmail at 55–65 units/s, and lockouts with
   a remembered ceiling spent about 4 of 8.5 minutes locked out while the
   ceiling ratcheted from 80 to 25 units/s. Gmail went on rejecting for about
   a minute after each trip, even at 25 units/s. So the binding constraint is
   the trailing minute, and the client paces to that
   (`QuotaPacing::Budget`, `MinuteBudget`, the default):
   - Every request, and every item of a batch, is charged its documented
     cost to a ledger shared by the client's clones before it is sent,
     failed attempts and retries included. The units charged in any
     trailing minute never exceed a budget B, and within the minute they go
     out evenly at B/60 per second, so nothing bursts after an idle spell.
   - B starts at 80% of the per-minute quota (4,800 at 6,000). When Gmail
     rate-limits a request, B becomes 85% of the ledger's trailing-minute
     spend at that moment, the learned ceiling (never below 600), and
     charges not yet sent are cancelled. Sending resumes as soon as enough
     units have left the window, with no fixed lockout. Rate limits on
     requests sent before that one are ignored. A rate limit on a request
     sent within a minute of it does not lower B again: Gmail is still
     counting what tripped it, so sending waits until all of that has left
     the window. That keeps a lingering rejection from ratcheting B down.
   - After each minute with no rate limit in which the client spent at
     least 90% of B, B rises by 5%, up to where it started. Idle minutes
     don't count.
   - `GmailClient::learned_budget` and `seed_budget` carry B between
     clients. `InitialSync` saves it per account in `sync_state`
     (`quota_budget`, with the quota it was learned under) after each page
     and at the end of a run, and a client that has not adapted yet starts
     at the saved value if the quota is unchanged. The first sync learns the
     ceiling with a single trip. Later ones start below it and probe up.
   - `ClientStats` (and sync `Progress`) report B and the trailing spend,
     shown in the CLI's progress line. Budget changes are logged at debug
     level.

   `GmailClientBuilder::quota_limit` (the CLI's `--rate`) fixes the rate
   instead, as a token bucket. On a simulated Gmail that trips at 2,750 a
   minute, the budget settles at about 2,350–2,850, averages about 2,450
   units a minute over 15 minutes and loses one call per trip, about one
   every five minutes. Against a Gmail that rejects everything for a minute
   after a trip, B is lowered once. Estimate for the 30-day test mailbox:
   about 5,000 units at roughly 2,400 a minute, a little over 2 minutes,
   including the first run's one trip.

   Rate-limit responses are retried on their own budget, starting at 5 s and
   growing to at most 60 s apart, three minutes in all, honouring
   `Retry-After`; server errors and timeouts back off from 0.5 s. If a rate
   limit still outlasts that, `InitialSync` reports the wait, pauses for 60 s
   and resumes from its checkpoint within the same run, a last resort now that
   the client lowers its budget on every rate limit. Rate-limited retries
   also wait for the ledger, so none goes out before the window has room.
   Batches log their size, latency and failures at debug level, and the
   client counts retried and rate-limited requests (`GmailClient::stats`)
   for progress reports.
2. **Incremental sync.** `HistorySync` polls `history.list` from the stored
   `historyId`, in pages of 500 records, once the initial sync is complete.
   Each page is applied in one transaction that also advances the stored
   `historyId`: to the page's newest record, or to the response's (current)
   `historyId` after the last page, so an interrupted poll resumes where it
   stopped.
   - Threads that gained or lost messages are fetched again (metadata, in
     batches) and replaced, which keeps the thread's snippet, date, message
     count and labels right; threads that are gone (404) are deleted.
   - Label additions and removals are applied to stored messages, and the
     store's triggers roll them up into `thread_labels` and the unread and
     starred flags. A thread fetched in the same page is already current, so
     its label changes are skipped. Changes to messages that are not stored
     (older than the cutoff) are ignored, except that a message moved to the
     Inbox has its thread fetched.
   - A change that refers to a user label the store does not know re-syncs
     the label list first.
   - Replaying a page is harmless: threads are replaced with the server's
     copy, and label changes add or remove rather than toggle.

   **Expired history.** If Gmail answers `404` (it keeps history for typically
   about a week), `InitialSync::reset` clears the `historyId`, the checkpoint and
   `backfill_complete`, keeping stored rows, and the initial sync runs again
   from scratch: a new `historyId` from the profile, then the listings with
   the configured cutoff. Threads whose history ID is unchanged are not
   fetched again. A sync that starts afresh records every thread it lists in
   `sync_seen_threads`; when it finishes, stored threads within its cutoff
   that it did not list are deleted (they are gone, or in Spam or Trash, which
   the sync does not list) and older ones are kept. A checkpoint written
   before this bookkeeping existed skips that sweep.

   **Scheduling.** `Scheduler` runs the initial sync until it is complete,
   then polls every 30 s while the app is active and every 3 minutes when idle
   (`SchedulerHandle::set_activity`), or at once on `poll_now` (for example on
   window focus). Transient failures back off exponentially from 5 s to
   5 minutes, waiting at least any `Retry-After`. Connectivity comes from the
   requests themselves: one that never gets an HTTP response marks the
   account offline, and the next success marks it online; there is no OS
   reachability check. An expired history triggers the resync at once. Other
   errors (sign-in, an exhausted daily quota, the store) stop the scheduler.
   It reports `SyncEvent`s (initial-sync progress, each poll's `Changes`,
   failures with the retry delay, connectivity) through a callback that
   `MailService` wraps. Gmail push needs Pub/Sub and a server, so desktop
   polling is the deliberate choice.
3. **Local changes.** User actions go through `OpsQueue` (`openmail-sync`'s `ops`
   module): label changes on threads or messages (archive, read and unread, star,
   important, spam, "move to"), trash and untrash, delete forever, and label
   create, rename, recolor and delete.
   - **Optimistic apply.** `enqueue` applies the op to the store in the same
     transaction that appends it to `pending_ops`, so queries (thread lists, label
     counts, thread detail) show it at once; the store's triggers keep
     `thread_labels` and the unread and starred flags right. The payload records
     what the op actually changed (labels added and removed per message, rows a
     delete removed, a label's previous name or color), so it can be reverted. A
     label created offline gets a temporary ID (`openmail-pending-<op>`) that
     later ops can use; when Gmail creates the label, the store and those ops
     switch to the real ID, and the ops wait until then.
   - **Undo.** A new op waits out an undo window (5 s by default, for 5.1's undo
     toast) before it is sent. Within it, `undo` deletes the op and reverts its
     change (undoing a label creation also undoes the ops that use the label).
     The worker claims an op (`sending`) before sending it, so an undo cannot race
     the send.
   - **Replay.** `OpsQueue::run` sends due ops in queue order per thread or label:
     an op waits while an earlier op on the same thread or label is waiting.
     Label changes with the same labels are coalesced into `messages.batchModify`
     calls (up to 1,000 messages each; threads with no stored messages use
     `threads.modify`), and a label change followed by its exact inverse (star,
     then unstar) cancels out without either being sent; the first waits while
     its inverse is in its undo window. Trash, untrash and deletes go one call per
     thread or message ("not found" counts as deleted). Requests go through the
     client's quota limiter. Network failures, server errors and rate limits
     retry with backoff (2 s doubling to 5 minutes, at least any `Retry-After`;
     an exhausted daily quota waits the maximum). Gmail rejecting an op (any
     other 4xx) drops it and reverts its local change; in a coalesced call that
     Gmail rejects, the ops are sent one at a time to find the culprit. Sign-in
     failures stop the worker with an error and keep the queue. Events (`Sent`,
     `CancelledOut`, `Retrying`, `Failed`) go to a callback that `MailService`
     wraps. On start the worker resends ops left `sending` by a crash; every send
     is idempotent.
   - **Conflicts: the server wins.** History and initial sync write Gmail's
     version of threads and labels as they are; then, in the same transaction,
     the queued ops on those threads (and, after a label sync, the label ops and
     the ops that use labels) are applied again on top, in queue order, so the
     user's pending change still shows until it is sent. Re-applying keeps the
     labels the op changed the first time as its own, so undo still reverts them.
     A sent op stays in the queue (`sent`) until a history sync that was started
     after it was sent finishes: Gmail data requested before the op was sent may
     not include it, so the op is re-applied over that data too, and data
     requested later is taken as is. History entries that echo a sent op change
     nothing (labels are added and removed, never toggled), and once it is gone,
     later changes from elsewhere win. Sent label creations and edits are not
     re-applied: the label is Gmail's from then on.
