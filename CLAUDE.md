# CLAUDE.md

Guidance for Claude Code sessions (and human contributors) working in this repository.
Read this file, [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) and your PR's entry in
[docs/ROADMAP.md](docs/ROADMAP.md) before writing code.

## Project overview

Openmail is an open-source, native desktop email client for Gmail, written in Rust.
It targets macOS first, then Linux and Windows. It treats Gmail as Gmail: labels,
threads, categories, filters, send-as aliases and Gmail search syntax all work the
way they do in Gmail web.

- **UI:** [GPUI](https://www.gpui.rs/) with `gpui-component` widgets. No Electron, no Tauri.
- **Backend:** the Gmail REST API (not IMAP), with `history.list` delta sync.
- **Message bodies:** HTML sanitized with `ammonia`, rendered in a sandboxed `wry`
  system webview that shows *only* the body. Email JavaScript is off and remote
  images are blocked by default.
- **Storage:** SQLite (bundled `rusqlite`) with FTS5; tokens in the OS keychain via `keyring`.
- **License:** `MIT OR Apache-2.0`.

The work is split into phases and small PRs, tracked in [docs/ROADMAP.md](docs/ROADMAP.md).

## Crate map

| Path | Crate | Role |
| --- | --- | --- |
| `crates/openmail-core` | `openmail-core` | Domain types: `Account`, `Label`, `Thread`, `Message`, `Draft`, IDs, errors. No I/O. |
| `crates/openmail-gmail` | `openmail-gmail` | OAuth2 (PKCE + loopback), typed Gmail REST client, batch requests, retries. |
| `crates/openmail-mime` | `openmail-mime` | Parse (`mail-parser`) and build (`mail-builder`) RFC 5322 messages. |
| `crates/openmail-store` | `openmail-store` | SQLite store, migrations, FTS5 index, queries. |
| `crates/openmail-sync` | `openmail-sync` | Sync engine, offline `pending_ops` queue, `MailService` on tokio. |
| `crates/openmail-ui` | `openmail-ui` | GPUI views, view-models, theme, keymap, webview bridge. |
| `crates/openmail-cli` | `openmail-cli` | Developer CLI (`login`, `labels`, `threads`, `show`, `sync`) for testing without the UI. |
| `apps/openmail` | `openmail` | The desktop binary. Wires `openmail-ui` to `openmail-sync`. |

Dependency direction: `core` ← `gmail`, `mime`, `store` ← `sync` ← `ui` ← `apps/openmail`.
`cli` may depend on anything except `ui`. Nothing depends on `ui` except the app.

## Threading model

- **GPUI owns the main thread** and its own executor. UI code never blocks on I/O.
- **All network, database and sync work runs in `MailService`** on a dedicated tokio
  runtime (in `openmail-sync`).
- The UI sends **commands** to `MailService` over a channel; `MailService` sends
  **events** back (`ThreadsChanged`, `SyncStatus`, …).
- `openmail-ui` bridges those events into GPUI entities with `cx.spawn`.
- Keep UI logic in plain view-model types so it is testable without a window.

## Commands

```bash
cargo build --workspace
cargo run -p openmail            # desktop app
cargo run -p openmail-cli -- …   # dev CLI (from PR 1.6)
cargo test --workspace
```

## Conventions

### Branches

`<type>/<pr-id>-<slug>`, e.g. `feat/2.3-initial-sync`, `ci/0.3-ci`, `docs/9.5-docs`.
One PR per roadmap item. Branch from an up-to-date `origin/main`
(`git fetch && git switch -c <branch> origin/main`). The branch prefix for each PR is
listed in its roadmap entry.

### Commits

- [Conventional Commits](https://www.conventionalcommits.org/): `feat(gmail): add history client`,
  `fix(store): …`, `test(mime): …`, `docs: …`, `chore: …`, `ci: …`, `refactor(ui): …`.
  Use the crate name without the `openmail-` prefix as the scope.
- Small and focused. **Every commit must build and pass `cargo test --workspace`.**
  A PR is typically 3–8 commits.
- No scope creep beyond the roadmap entry. Note follow-ups in the PR body instead.

### Gates (run before every push)

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Workspace lints live in the root `Cargo.toml` and every crate opts in with
`[lints] workspace = true`. `unsafe_code` is denied; if a crate truly needs FFI,
allow it on the smallest possible item with a comment explaining why.

### Dependencies

Declare shared versions once in `[workspace.dependencies]` in the root `Cargo.toml`
and use `foo.workspace = true` in crates. Add a dependency in the PR that first
needs it. Pin `gpui` and `gpui-component` to exact (`=`) versions and upgrade them
only in dedicated PRs.

### Tests

- **Never call the real Gmail API in tests.** Use `wiremock` fixtures for HTTP,
  a `.eml` corpus for MIME, and `insta` snapshots where output is large.
- Test UI behaviour through view-model logic, not pixels.
- Tests must not need network access, a keychain or a Google account.

### Secrets

- Never commit OAuth client secrets, tokens, or real mail.
- The Google OAuth client ID comes from the `OPENMAIL_GOOGLE_CLIENT_ID` environment
  variable or from user config. It is never hard-coded.

### Pull requests

1. Tick your PR's checkbox in `docs/ROADMAP.md` as its own commit
   (`docs(roadmap): tick <ID>`).
2. Push and open the PR with `gh pr create`. Title: the Conventional Commit form of
   the roadmap title, e.g. `feat(sync): initial sync (2.3)`.
3. PR body format:

   ```markdown
   Roadmap: <ID> <title>

   ## Summary
   <what changed and why, 2–5 bullets>

   ## Commits
   - <type(scope): subject>
   - …

   ## Test plan
   - [x] cargo fmt --check
   - [x] cargo clippy --workspace --all-targets -- -D warnings
   - [x] cargo test --workspace
   - [ ] <manual checks, if any>

   ## Follow-ups
   <anything deliberately left out, or "None">
   ```

4. Get CI green on macOS, Linux and Windows, then stop. Do not start the next PR.

## Per-PR session prompt

Every PR runs in its own Claude Code session, in its own worktree of the repo, with
this prompt (only the ID and title change):

```
Implement PR <ID> "<title>" for Openmail (github.com/charlie-niekirk/openmail).

1. Read CLAUDE.md, docs/ARCHITECTURE.md, and the "<ID>" entry in docs/ROADMAP.md
   (plus the merged PRs it depends on) before writing code.
2. git fetch && branch from origin/main as <type>/<ID>-<slug>.
3. Build it in small, concise Conventional Commits; every commit must build and
   pass tests. No scope creep beyond the roadmap entry. Note follow-ups in the PR.
4. Before pushing: cargo fmt --check, cargo clippy --workspace --all-targets
   -- -D warnings, cargo test --workspace.
5. Tick the PR's checkbox in docs/ROADMAP.md (as its own commit).
6. Push and open the PR with `gh pr create` (summary, commits, test plan).
   Get CI green on all three OSes, then stop. Do not start the next PR.
```

Example: `Implement PR 2.3 "Initial sync" for Openmail …`.
PRs marked **∥** in the roadmap can run in parallel with their siblings once their
shared dependency has merged. Merge a PR before starting any session that depends on it.
