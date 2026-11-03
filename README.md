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

## Quick start (Windows client, issue #7)

```
cargo build --release
target\release\activity-daemon.exe run
```

First `run` creates a fully commented config template with defaults at
`%APPDATA%\activity-daemon\config.toml` (override with
`--config <path>`) and uses the defaults. Edit at least `[sync]`
(`server_url`, `device_name`) once the hub exists.

- **Where data lives**: `%APPDATA%\activity-daemon\data` by default -
  `activity.db` (SQLite, WAL), `media\frames` + `media\audio` (rolling
  retention budgets), `heartbeat` (liveness, rewritten every 5 s),
  `daemon.lock` (single instance), `daemon.log` (+ `logs\audio.log`).
  Stop with Ctrl+C; the window watcher flushes its open session.
- **`status`** (`--json` for machines): daemon running/stale/not-running
  from the heartbeat, current foreground app, active time today (window
  sessions since local midnight, AFK excluded), unsynced event count,
  db size, frames/audio bytes vs their rolling budgets.
- **`startup install`** registers a hidden Task Scheduler task
  "ActivityDaemon" (logon trigger, restart on failure 3x/1 min, working
  dir = data dir) and starts it; `startup status` / `startup uninstall`
  manage it; `startup install --dry-run` prints the task XML.
- **`record start` / `record stop`**: manual mic-only recording for
  in-person meetings (writes the `record.json` control file the audio
  watcher consumes; see watcher-audio crate docs). The global hotkey
  (`ctrl+shift+alt+m`) does the same.

Server side (WSL hub, ingest endpoints, tailscale wiring): see
[docs/sync.md](docs/sync.md).

## Docs

- docs/needs.md - owner need profile, source of truth for design decisions
- docs/process.md - dev loop, release flow, post-milestone retro
- docs/data-pipeline.md - v0.2 processing plan (classifier, summaries, export, STT, MCP)
- docs/lessons.md - dev gotchas log
- AGENTS.md - rules for AI collaborators
