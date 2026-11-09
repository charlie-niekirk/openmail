# Openmail

A fast, native desktop email client for Gmail, written in Rust.

> **Status:** early development. Nothing usable yet. See the [roadmap](docs/ROADMAP.md) for what is planned and what is done.

## Documentation

- [Roadmap](docs/ROADMAP.md): phases and PRs, with progress checkboxes.
- [Architecture](docs/ARCHITECTURE.md): crates, design decisions, threading and sync models.
- [CLAUDE.md](CLAUDE.md): contributor conventions (branches, commits, gates, PRs).

## Goals

- **Native and fast**: GPU-accelerated UI built with [GPUI](https://www.gpui.rs/), no Electron and no Tauri.
- **Gmail-first**: labels, threads, categories, filters, send-as aliases and Gmail search syntax, via the Gmail REST API.
- **Offline-friendly**: local SQLite cache with full-text search and an offline action queue.
- **Private by default**: remote images blocked, HTML sanitized, no email JavaScript.
- **Cross-platform**: macOS first, then Linux and Windows.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
