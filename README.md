# activity-daemon

Personal activity collection daemon for a single owner: independent watcher
plugins collect raw activity data (foreground windows, AFK state, screen
frames, meeting audio) into a local-first SQLite store on Windows, then batch
forward everything to a central hub on the owner's WSL box over Tailscale -
no byte ever leaves the owner's tailnet and no third-party service is
involved. Raw data is collected first; classification, summaries, export and
AI analysis are later refinements on the central store (see
docs/data-pipeline.md). Design contracts live in the ADR (issue #16) and the
owner need profile (docs/needs.md) - every feature must trace to a stated
need.

## Crates

| Crate | Type | Purpose |
|---|---|---|
| `activity-core` (crates/core) | lib | SQLite event store (WAL, rusqlite bundled) + TOML config |
| `watcher-window` (crates/watcher-window) | lib | window/AFK session coalescing (Windows hooks land in #4) |
| `watcher-screen` (crates/watcher-screen) | lib | rolling media retention planner (capture lands in #5) |
| `watcher-audio` (crates/watcher-audio) | lib | meeting audio contract skeleton (#6) |
| `server` (crates/server) | bin | WSL central ingestion endpoint (axum), deploy via Containerfile |
| `activity-cli` (crates/cli) | bin | `activity-daemon` CLI: run / status / startup / record |

## Build

Prerequisite: a stable Rust toolchain (edition 2021).

```
cargo build --release          # everything
cargo test --workspace         # unit + integration tests
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

Windows client: `target/release/activity-daemon.exe`.
Central server (on WSL, via podman): `podman build -t activity-server . && podman run -d -p 8080:8080 -v /srv/activity:/data activity-server`.

## Docs

- docs/needs.md - owner need profile, source of truth for design decisions
- docs/process.md - dev loop, release flow, post-milestone retro
- docs/data-pipeline.md - v0.2 processing plan (classifier, summaries, export, STT, MCP)
- docs/lessons.md - dev gotchas log
- AGENTS.md - rules for AI collaborators
