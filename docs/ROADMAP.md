# Roadmap

Openmail is built in 10 phases of small PRs. Each entry below is one PR, with its ID,
title, suggested branch name and scope. See [CLAUDE.md](../CLAUDE.md) for how a PR is
built and [ARCHITECTURE.md](ARCHITECTURE.md) for the design.

**How to read this file**

- Run PRs in order unless marked **∥**. A **∥** PR can run in parallel with its
  siblings in the same phase once the PR they share as a dependency has merged
  (usually the nearest preceding PR without **∥**).
- Each PR runs in its own Claude Code session, in its own worktree, using the
  session prompt in [CLAUDE.md](../CLAUDE.md#per-pr-session-prompt).
- Merge a PR before starting any session that depends on it.
- Tick the checkbox in the PR itself, as its own commit (`docs(roadmap): tick <ID>`).
- Branch names follow `<type>/<pr-id>-<slug>`; the slug shown is a suggestion.

---

## Phase 0 — Foundation

- [x] **0.1 Create repo** — `chore/0.1-create-repo`
  - Public repo `charlie-niekirk/openmail` with README stub, LICENSE-MIT,
    LICENSE-APACHE and `.gitignore` on `main`.
- [x] **0.2 Workspace and conventions** — `chore/0.2-workspace`
  - Workspace `Cargo.toml` with shared lints, `rust-toolchain.toml`, `rustfmt.toml`.
  - Empty crates under `crates/` and the `apps/openmail` binary.
  - `CLAUDE.md`, `docs/ROADMAP.md` (this file) and `docs/ARCHITECTURE.md`.
- [x] **0.3 CI** ∥ — `ci/0.3-ci`
  - GitHub Actions matrix on macOS, Ubuntu and Windows: fmt, clippy, test and
    `cargo-deny` (licenses and advisories), with rust-cache.
  - Dependabot, and issue and PR templates.
  - Install the Linux system packages GPUI and wry need: xkbcommon, wayland, vulkan,
    x11-xcb, fontconfig, webkit2gtk-4.1, gtk3.
- [x] **0.4 GPUI hello window** ∥ — `feat/0.4-gpui-window`
  - `apps/openmail` opens a themed `gpui-component` window with a macOS transparent
    titlebar.
  - Pin `gpui` and `gpui-component` to compatible revisions.
  - Must build in CI on all three OSes.
- [x] **0.5 Webview spike (risk check)** — `feat/0.5-webview-spike`
  - Embed a `wry` child webview inside a GPUI window, positioned at a layout rect,
    showing static HTML. Check `gpui-component`'s webview feature first.
  - Prove it on macOS; record findings for Windows and Linux in `ARCHITECTURE.md`.
  - Linux Wayland child webviews are the known risk. If they fail, fall back to
    XWayland, or to text-first rendering on Wayland.

## Phase 1 — Gmail API foundation (no UI)

- [x] **1.1 Core domain types** — `feat/1.1-core-types`
  - IDs, `Label` (system or user, colors, visibility), `Thread`, `Message`, `Address`,
    `Attachment`, `Draft`, and error types.
- [x] **1.2 OAuth2** ∥ — `feat/1.2-oauth`
  - Installed-app flow with PKCE and a loopback redirect on `127.0.0.1:<random>`;
    opens the system browser.
  - Token refresh; tokens stored in the OS keychain via `keyring`.
  - Scopes: `gmail.modify` and `gmail.settings.basic`.
  - Client ID from `OPENMAIL_GOOGLE_CLIENT_ID` or user config (bring your own client).
- [x] **1.3 Gmail REST client** ∥ — `feat/1.3-gmail-client`
  - Typed serde models for labels, threads.list/get, messages.get (metadata, full,
    raw) and history.list.
  - Pagination; exponential backoff on 429 and 5xx.
  - `wiremock` tests.
- [x] **1.4 Batch and mutations** — `feat/1.4-batch-mutations`
  - `multipart/mixed` batch endpoint (≤ 50 requests per batch).
  - messages/threads `modify`, `batchModify`, trash/untrash, delete, labels CRUD.
- [x] **1.5 MIME** ∥ — `feat/1.5-mime`
  - Parse raw messages into text, HTML, inline `cid:` parts and attachments.
  - Build outgoing mail with alternatives and attachments; encode as base64url.
  - Tests against a `.eml` fixture corpus.
- [x] **1.6 Dev CLI** — `feat/1.6-dev-cli`
  - `openmail-cli login | labels | threads <label> | show <thread>`.
  - First end-to-end check against a real account (by hand, never in tests).

## Phase 2 — Local store and sync engine

- [x] **2.1 SQLite store** — `feat/2.1-sqlite-store`
  - Tables: accounts, labels, threads, messages, message_labels, bodies, attachments,
    drafts, sync_state, pending_ops.
  - FTS5 index, WAL mode, migrations, per-account data directory.
- [ ] **2.2 Queries and benchmarks** — `feat/2.2-queries-bench`
  - Keyset-paginated threads by label, unread counts per label, thread detail.
  - Criterion benchmark over 100k synthetic threads; first page loads in < 5 ms.
- [x] **2.3 Initial sync** — `feat/2.3-initial-sync`
  - Labels → Inbox → backfill, using batched metadata fetches.
  - Checkpointed `historyId` and resumable progress.
- [ ] **2.4 Incremental sync** — `feat/2.4-incremental-sync`
  - Apply `history.list` entries (messages added or deleted, labels added or removed).
  - Full resync when the `historyId` has expired (404).
  - Polling scheduler with online and offline detection.
- [ ] **2.5 Offline ops queue** — `feat/2.5-ops-queue`
  - Optimistic local apply, persisted queue, replay with retries, conflict handling.
- [ ] **2.6 MailService** — `feat/2.6-mail-service`
  - Tokio runtime, command and event channels.
  - Lazy body fetch with prefetch.
  - The public API the UI consumes.

## Phase 3 — App shell and read-only UI

- [ ] **3.1 UI architecture** — `feat/3.1-ui-architecture`
  - `openmail-ui` crate, bridge from tokio events to GPUI entities, `AppState`.
  - Light and dark themes that follow the system, typography, the window frame.
- [ ] **3.2 Onboarding** — `feat/3.2-onboarding`
  - "Sign in with Google" screen and initial-sync progress.
- [ ] **3.3 Sidebar** ∥ — `feat/3.3-sidebar`
  - System labels, nested user-label tree (split on `/`), label colors, unread counts,
    collapse state.
- [ ] **3.4 Thread list** ∥ — `feat/3.4-thread-list`
  - Virtualized `uniform_list` with infinite scroll.
  - Rows: senders, subject, snippet, date, label chips, star and importance markers,
    unread styling, attachment icon.
- [ ] **3.5 Conversation view** ∥ — `feat/3.5-conversation-view`
  - Native header, collapsed and expanded messages, participants, label chips.
  - Plain-text body for now.
- [ ] **3.6 Layout** — `feat/3.6-layout`
  - Resizable 3-pane layout with persisted sizes.
  - Empty, loading, error and offline states.

## Phase 4 — Message rendering

- [ ] **4.1 Webview component** — `feat/4.1-webview`
  - Production version of the 0.5 spike: lifecycle, resizing, scroll integration with
    the conversation, focus handling.
- [ ] **4.2 Sanitization and privacy** — `feat/4.2-sanitize-privacy`
  - `ammonia` email allowlist, strict CSP, JavaScript off.
  - Remote images blocked behind "show images" (per message, or always for a sender).
  - `cid:` rewriting; link clicks open in the system browser with the real URL shown.
- [ ] **4.3 Body polish** — `feat/4.3-body-polish`
  - Auto-height through a trusted injected script only.
  - Dark-mode adaptation; quoted-text collapsing ("…").
- [ ] **4.4 Attachments** ∥ — `feat/4.4-attachments`
  - List, on-demand download, macOS Quick Look, open with the default app, save as.

## Phase 5 — Triage actions and labels

- [ ] **5.1 Core actions** — `feat/5.1-core-actions`
  - Archive, trash, delete forever, read/unread, star, important, spam/not spam.
  - Optimistic, with an undo toast that delays the op by about 5 s.
- [ ] **5.2 Label picker** ∥ — `feat/5.2-label-picker`
  - Searchable multi-select popover, "Move to" (apply a label and remove Inbox),
    create a label inline.
- [ ] **5.3 Multi-select and bulk** ∥ — `feat/5.3-bulk-actions`
  - Shift-click and cmd-click selection, select all in view, bulk actions through
    `batchModify`.
- [ ] **5.4 Label management** — `feat/5.4-label-management`
  - Create, rename, nest and delete labels; color picker limited to Gmail's palette;
    visibility settings.
- [ ] **5.5 Snooze (local)** — `feat/5.5-snooze`
  - The Gmail API does not expose snooze: use a hidden `Openmail/Snoozed` label and a
    local scheduler that returns the thread to Inbox.
  - Document that these snoozes do not appear under Gmail web's "Snoozed".

## Phase 6 — Compose

- [ ] **6.1 Composer UI** — `feat/6.1-composer`
  - Window or inline pane, recipient chips with autocomplete from local address
    history, subject, plain-text body.
- [ ] **6.2 Reply, reply-all and forward** — `feat/6.2-reply-forward`
  - Correct `In-Reply-To` and `References` headers, `threadId`, quoting.
  - Send-as alias picker and signatures (`settings.sendAs`).
- [ ] **6.3 Drafts** ∥ — `feat/6.3-drafts`
  - Debounced autosave to the Gmail drafts API, resume, discard, offline drafts.
- [ ] **6.4 Attachments and send** ∥ — `feat/6.4-attachments-send`
  - File picker, drag and drop, the 25 MB limit, upload sends.
  - Outbox for offline, undo send.
- [ ] **6.5 Rich-text editor** — `feat/6.5-rich-text`
  - In-house rich-text model (bold, italic, underline, link, lists, quote) that
    outputs HTML plus a text/plain alternative.
  - Large; may split into 6.5a and 6.5b.

## Phase 7 — Search and Gmail power features

- [ ] **7.1 Search** — `feat/7.1-search`
  - Gmail operator syntax sent as `q` when online; local FTS5 fallback offline.
  - Suggestions for contacts, labels and operators.
- [ ] **7.2 Inbox categories** ∥ — `feat/7.2-categories`
  - Primary, Social, Promotions, Updates and Forums tabs (the `CATEGORY_*` labels).
  - Important-first and Starred-first modes.
- [ ] **7.3 Filters** ∥ — `feat/7.3-filters`
  - List, create and delete Gmail filters (`settings.filters`); "Filter messages like this".
- [ ] **7.4 Account settings** ∥ — `feat/7.4-account-settings`
  - Send-as aliases and signature editor, vacation responder.

## Phase 8 — Productivity and multi-account

- [ ] **8.1 Gmail-compatible shortcuts** — `feat/8.1-shortcuts`
  - j/k, o, u, e, #, !, r, a, f, c, s, x, l, v, `g i`, `/`, `?`.
  - Customizable keymap and a cheat-sheet overlay.
- [ ] **8.2 Command palette** ∥ — `feat/8.2-command-palette`
  - ⌘K for actions, label navigation and search.
- [ ] **8.3 Multi-account** — `feat/8.3-multi-account`
  - Account switcher, unified inbox, per-account accent colors.
- [ ] **8.4 Notifications** ∥ — `feat/8.4-notifications`
  - Native notifications, unread badge on the dock or taskbar, background sync while
    the window is closed.
- [ ] **8.5 Settings window** — `feat/8.5-settings`
  - Appearance, density, images policy, sync window, notifications, keymap.

## Phase 9 — Cross-platform polish and release

- [ ] **9.1 Linux** ∥ — `feat/9.1-linux`
  - Secret Service keyring, WebKitGTK, Wayland/X11 behaviour, `.desktop` file.
- [ ] **9.2 Windows** ∥ — `feat/9.2-windows`
  - WebView2 runtime check, Credential Manager, DPI and window chrome.
- [ ] **9.3 Packaging and releases** — `build/9.3-packaging`
  - `cargo-packager`: macOS `.app`/`.dmg` (codesigned and notarized), Windows MSI,
    Linux AppImage, `.deb` and Flatpak.
  - Tag-triggered release workflow, `release-please` for changelogs.
- [ ] **9.4 Auto-update** — `feat/9.4-auto-update`
  - Signed update manifest using the `cargo-packager` updater.
- [ ] **9.5 Docs** — `docs/9.5-docs`
  - User guide, "bring your own Google OAuth client" guide, CONTRIBUTING.

---

## Verification

- **Every PR:** CI green on macOS, Linux and Windows (fmt, clippy, tests, cargo-deny).
- **Phase 1:** `openmail-cli login`, then `labels` and `threads INBOX`, against a real
  test Gmail account.
- **Phase 2:** CLI `sync` against a real account. Change a label in Gmail web and
  confirm it appears locally within one poll. Run the criterion benchmark.
- **Phase 3 onward:** `cargo run -p openmail` on macOS. Check each feature by hand
  against Gmail web: label counts match, archiving in Openmail reflects in Gmail web
  and the reverse, and sent replies thread correctly.
- **Phase 9:** install the signed `.dmg`, MSI and AppImage on clean machines and VMs,
  and test the auto-update path from N-1 to N.

## Risks and mitigations

- **Google OAuth verification.** `gmail.modify` is a *restricted* scope. A public
  release needs Google verification plus a yearly CASA security assessment. Until then
  the app runs as an unverified "testing" app, limited to 100 test users who see a
  warning screen. *Mitigation:* support a bring-your-own client ID from day one (1.2)
  and document it (9.5).
- **GPUI is pre-1.0.** *Mitigation:* pin exact revisions and upgrade deliberately in
  dedicated PRs. Keep UI logic in view-models so it stays testable without GPUI.
- **Webview embedding.** *Mitigation:* spike 0.5 covers this first. Linux Wayland
  child webviews are the weakest point.
- **Rich-text editing** has no ready-made GPUI widget. *Mitigation:* isolated to 6.5;
  the composer ships with plain text first.
- **Accessibility.** GPUI's screen-reader support (AccessKit) is still immature.
  *Mitigation:* track it upstream and add labels where the API allows.
- **No push notifications for desktop.** Gmail push requires Pub/Sub plus a server.
  *Mitigation:* poll `history.list`, which is cheap in quota terms.
- **Release signing.** 9.3 needs an Apple Developer ID and a Windows code-signing
  certificate stored as repo secrets, which the maintainer must provide.
