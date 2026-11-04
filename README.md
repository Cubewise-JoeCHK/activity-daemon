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
| `activity-cli` (crates/cli) | bin | `activity-daemon` CLI: run / status / startup / record / update / usage |

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

## Updates (GitHub Releases, issue #17)

While the daemon runs, it checks
`https://api.github.com/repos/Cubewise-JoeCHK/activity-daemon/releases/latest`
for a newer release every `update_interval_s` (default 86400 s = daily) and
self-updates. The check is unauthenticated (60 requests/hour per IP - a
daily check is comfortably inside it) and failures (network down, 403 rate
limit, malformed JSON, no releases yet) are logged and ignored: the daemon
keeps running and retries after the interval.

Config (`[sync]` in config.toml):

```
auto_update = true          # set false to disable the automatic check
update_interval_s = 86400   # seconds between checks (daily)
```

`auto_update` only gates the automatic check - the manual `update`
subcommand always works.

What happens on a newer release (safety order matters):

1. Download `activity-daemon-windows-x86_64.exe` + its `.sha256` companion
   from the release into `%APPDATA%\activity-daemon\data\update\`
   (`<tag>.exe.tmp`).
2. Verify the sha256 (case-insensitive hex). On mismatch the download is
   deleted and the update aborts - the old version keeps running.
3. Rotate the binary: any previous `<exe>.old` is removed (exactly one
   rollback copy exists between updates), the running exe is renamed to
   `<exe>.old` (allowed on Windows while it runs), the download is renamed
   over the exe path. If that last rename fails, `.old` is renamed back -
   the daemon is never left without a working exe.
4. Shut down gracefully (same path as Ctrl+C: workers join, the window
   watcher flushes its open session) and relaunch - via the "ActivityDaemon"
   Task Scheduler task when installed, otherwise the new exe is spawned
   detached. The new version starts, finds no newer release, and the cycle
   repeats on the next release.

Manual commands: `activity-daemon update` (check now + apply; when the
daemon is running it is asked to restart gracefully via the
`data\update\restart.request` control file and relaunches within a second)
and `activity-daemon update check` (report only: current version, latest
release, whether it would apply).

**Rollback**: stop the daemon (Ctrl+C or `startup uninstall`) and copy the
old binary back over the new one - the rollback copy sits next to your exe
(e.g. `C:\tools\activity-daemon.exe.old`):

```
copy /y C:\tools\activity-daemon.exe.old C:\tools\activity-daemon.exe
```

**SmartScreen note**: the release binaries are unsigned, so Windows
SmartScreen may warn the first time a new binary runs ("More info" ->
"Run anyway"). This also applies to manually restored `.old` copies.

## Usage log (per-feature counts, issue #27)

Every **user-initiated** action writes one event into the `usage` bucket
through the normal core insert (`data = {"feature": ..., "action": ...}`,
`duration_s = 0`, `device = sync.device_name`). The rows live in the
local-first SQLite store like any event and reach the WSL hub through the
regular sync forwarder, so server-side aggregation stays possible later.
Automatic features (auto-update scheduled checks, sync, capture, retention)
NEVER log - otherwise they would fake usage.

`activity-daemon usage` prints per feature+action counts over
today / 7d / 30d / all-time from the local store (`--json` for machines);
"today" starts at local midnight. The taxonomy is fixed and documented:

| feature | action | logged when |
|---|---|---|
| `daemon` | `start` | every daemon boot (`run`) |
| `status` | `view` | every `status` invocation |
| `meeting-recording` | `manual-start` | `record start` |
| `meeting-recording` | `manual-stop` | `record stop` |
| `startup` | `install` | `startup install` (non-dry-run) |
| `startup` | `uninstall` | `startup uninstall` (non-dry-run; the `startup status` query does NOT log) |
| `update` | `apply` | manual `update` only - the automatic scheduled check NEVER logs |
| `update` | `check` | manual `update check` only - never the scheduled check |
| `usage` | `view` | the `usage` report itself (honest meta-signal) |

Instrumentation is fire-and-forget: a usage-insert failure prints a warning
and never fails the command you actually ran.

## Docs

- docs/needs.md - owner need profile, source of truth for design decisions
- docs/process.md - dev loop, release flow, post-milestone retro
- docs/data-pipeline.md - v0.2 processing plan (classifier, summaries, export, STT, MCP)
- docs/lessons.md - dev gotchas log
- AGENTS.md - rules for AI collaborators
