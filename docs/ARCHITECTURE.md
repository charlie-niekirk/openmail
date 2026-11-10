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

Spike 0.5 proves the embedding on macOS and records findings for Windows and Linux
here. Linux Wayland child webviews are the known risk; fallbacks are XWayland or
text-first rendering on Wayland.

### Storage: SQLite with FTS5

A local SQLite database (bundled `rusqlite`, so FTS5 is always available) caches
accounts, labels, threads, messages, message-label links, bodies, attachments,
drafts, sync state and pending operations. FTS5 powers offline search. WAL mode lets
the UI read while sync writes. Thread lists use keyset pagination; the target is
the first page of a label in under 5 ms with 100k threads.

### Secrets: OS keychain via `keyring`

OAuth refresh and access tokens are stored in the OS credential store (macOS
Keychain, Windows Credential Manager, Secret Service on Linux) via `keyring`, never
on disk in plain text and never in the repo. The Google OAuth client ID comes from
`OPENMAIL_GOOGLE_CLIENT_ID` or user config, so anyone can bring their own client
while the app is unverified.

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
