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
| `watcher-screen` (crates/watcher-screen) | lib | per-monitor JPEG frame capture (content deduped) + rolling retention planner (#5) |
| `watcher-audio` (crates/watcher-audio) | lib | meeting audio contract skeleton (#6) |
| `server` (crates/server) | bin | WSL central ingestion endpoint + read-only web UI (axum), deploy via Containerfile |
| `activity-cli` (crates/cli) | bin | `activity-daemon` CLI: run / status / startup / install / ensure-run / record / update / usage |
| `activity-update` (crates/update) | lib | shared release check + sha256 verification for both updaters (#17, #32) |
| `activity-server-update` (crates/server-update) | bin | `activity-server-update`: hub auto-updater on the WSL host (check / apply / watch / install / status / rollback, #32) |

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
  `activity.db` (SQLite, WAL), `media\frames` + `media\audio` (media
  pass-through, sync contract v2 #40: files are deleted locally once the
  hub has durably accepted both the file (checksum-verified) and its index
  row), `heartbeat` (liveness, rewritten every 5 s), `daemon.lock` (single
  instance), `daemon.log` (+ `logs\audio.log`). Stop with Ctrl+C; the
  window watcher flushes its open session. The rolling budgets
  (`screen.max_total_mb` / `max_age_days`, audio equivalents) are a backstop
  only now: they matter while the hub is unreachable, not in day-to-day
  operation.
- **Multi-monitor**: `screen.monitors` picks what the frame watcher
  captures - `all` (default, one frame per connected monitor) or
  `primary` (the v0.1 behavior). Frame files carry their monitor in the
  name (`media\frames\<Y>\<M>\<D>\<HHMMSS>_<monitor>_<sha16>.jpg`) and
  each monitor dedups against its own previous frame, so a static
  secondary screen costs nothing while it is unchanged. All monitors
  share the `screen.max_total_mb` budget: with two similar screens the
  same budget holds roughly half the history - raise it to compensate.
- **`status`** (`--json` for machines): daemon running/stale/not-running
  from the heartbeat, current foreground app, active time today (window
  sessions since local midnight, AFK excluded), unsynced event count,
  db size, frames/audio bytes vs their rolling budgets.
- **`startup install`** registers autostart with a no-admin fallback chain
  (issue #29): the hidden Task Scheduler task "ActivityDaemon" (logon
  trigger, restart on failure 3x/1 min, working dir = data dir), then a
  minimal task XML when the scheduler rejects the rich settings, then a
  Startup-folder `activity-daemon.vbs` when `schtasks` is blocked
  (e.g. "Access is denied." under corporate policy). It prints which
  mechanism won and its trade-off; `startup status` / `startup uninstall`
  manage all of them; `startup install --dry-run` previews everything.
- **`record start` / `record stop`**: manual mic-only recording for
  in-person meetings (writes the `record.json` control file the audio
  watcher consumes; see watcher-audio crate docs). The global hotkey
  (`ctrl+shift+alt+m`) does the same.

Server side (WSL hub, ingest endpoints, tailscale wiring): see
[docs/sync.md](docs/sync.md).

## Server web UI (read-only)

The hub ships a small web UI - open `http://<hub>:8080/` from any tailnet
device. The design is "Graphite" (a Teams-shaped mono light theme, winner of
a design contest; the mockup lives next to the repo in the workspace's
`design-contest/` folder). Assets are embedded in the binary at compile
time (`crates/server/assets/`) - nothing is fetched from outside the
tailnet, and the container image stays self-contained.

- **Machine view** (`#machine`): one card per reporting device - presence
  derived from event recency (active < 2 min, away < 1 h, otherwise
  offline), active time today, foreground app, a latest-arrivals feed with
  per-bucket tags, plus hub facts (version, uptime, stored media bytes).
  The hub has no client heartbeat file, so "last seen" = the newest event
  arrival per device.
- **Calendar view** (`#calendar`): Teams-style work-week grid of window
  sessions + AFK spans (07:00-21:00, now-line, mini month with activity
  dots). Shading classifies apps presentation-only (code/terminals dark,
  meetings, browsers, comms); the authoritative classifier is the planned
  v0.2 config-driven rules engine, which will replace the heuristics.
- **JSON APIs** (read-only, same open precedent as `GET /health`; writes
  stay token-guarded):
  - `GET /api/machines` - per-device summary + the 30 newest events each.
  - `GET /api/calendar?from=YYYY-MM-DD&to=YYYY-MM-DD[&device=...]` - day
    bucketed sessions/AFK, empty days included, range cap 62 days; without
    `device` the busiest device in the range wins. AFK spans come from the
    `state: "active"` events (start + duration); the `state: "afk"`
    markers are ignored.

Day boundaries ("today", calendar bucketing) use the server's local
timezone - a single-owner setup shares one timezone, and window events
already carry the local offset. The UI auto-refreshes every 30 s; the
refresh button forces it. New core queries used by the endpoints:
`device_overview`, `query_events_in`, `latest_events`,
`count_device_events_since`, `counts_by_device_bucket` (all instant-safe
across mixed UTC/local offsets).

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
   detached.
5. Verify the relaunch (issue #37): the exiting daemon waits up to 15 s for
   the new instance's first heartbeat beat - the pid must CHANGE, because
   the old daemon's final shutdown beat is fresh but carries the old pid.
   When the new exe never comes up (e.g. an EDR kills it on sight -
   observed live on 2026-11-06, silently killing capture for the rest of
   the day), the update rolls back automatically: the failed exe moves to
   `<exe>.bad`, the `.old` copy returns to the canonical path, the previous
   version is relaunched and the rollback is printed loudly. The watchdog
   service (issue #36) stays as the second layer of defence.

Manual commands: `activity-daemon update` (check now + apply; when the
daemon is running it is asked to restart gracefully via the
`data\update\restart.request` control file and relaunches within a second)
and `activity-daemon update check` (report only: current version, latest
release, whether it would apply).

**Rollback**: a failed relaunch rolls itself back automatically (step 5
above; the failed exe is kept as `<exe>.bad`). Manual rollback still works:
stop the daemon (Ctrl+C or `startup uninstall`) and copy the old binary
back over the new one - the rollback copy sits next to your exe
(e.g. `C:\tools\activity-daemon.exe.old`):

```
copy /y C:\tools\activity-daemon.exe.old C:\tools\activity-daemon.exe
```

**SmartScreen note**: the release binaries are unsigned, so Windows
SmartScreen may warn the first time a new binary runs ("More info" ->
"Run anyway"). This also applies to manually restored `.old` copies.

## Hub updates (`activity-server-update`, issue #32)

The WSL hub updates itself the same way, from the same releases: a small
updater binary runs **on the WSL host** (a container cannot swap its own
image), checks `releases/latest` daily, downloads the sha256-verified
`activity-server-linux-x86_64` asset, builds a thin image from it (no
compilation on the hub), smoke tests `--version`, swaps the container with
its original run args plus `--restart=on-failure`, and health-gates the
result with automatic rollback to the previous image. `install` sets up a
systemd **user** service (daily `watch` loop, linger); the watch tick also
self-heals a container that vanished mid-swap from the saved state.

```
activity-server-update check       # report only
activity-server-update             # apply now
activity-server-update install     # systemd user service, daily checks
activity-server-update status      # service, saved state, running version
activity-server-update rollback    # back to the saved rollback image
```

The current running version is reported by `/health` (`version` field) and
stamped as `org.opencontainers.image.version` on every updater-built
image; the server drains SIGTERM so `podman stop` mid-update is graceful.
Full runbook (adoption of a manual deployment, manual rollback): see
[docs/sync.md](docs/sync.md).

## Startup methods (autostart fallback chain, issue #29)

On a real second machine, `schtasks /create /f /tn ActivityDaemon /xml ...`
returned **"Access is denied."** - corporate GPO/EDR restrictions and
standard (non-admin) user accounts both block task creation. `startup
install` therefore walks a fallback chain, and **every step needs no admin**:

1. **Full task XML** - the preferred mechanism: hidden task "ActivityDaemon"
   with a logon trigger and `RestartOnFailure` (3 retries / 1 minute), so a
   crashed daemon is restarted automatically.
2. **Minimal task XML** - when the scheduler (or a policy) rejects the rich
   settings: the same logon trigger and least-privilege principal without
   `Hidden` and `RestartOnFailure`. Trade-off: no crash auto-restart.
3. **Startup-folder fallback** - when `schtasks` refuses twice:
   `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup\activity-daemon.vbs`
   launches the daemon windowless at logon
   (`Wscript.Shell.Run """<exe>"" run", 0, False` - no console flash).
   Trade-off: no crash auto-restart - a crashed daemon stays down until the
   next logon.

`install` prints which mechanism was installed and its trade-off.
`startup status` checks both artifacts (the scheduled task and the Startup
folder) and reports the effective mechanism. `startup uninstall` removes
the task and both fallback files (including a legacy
`activity-daemon.cmd` from older installs).

## One-command install (watchdog supervisor, issue #36)

`activity-daemon install` takes a machine from bare install to fully
supervised capture. This exists because a real machine silently lost a
full afternoon on 2026-11-06: the daemon died during an auto-update
relaunch and nothing noticed. The layers, in order:

1. **Config** - `resolve` writes the template on first use; `--device
   <name>`, `--server-url <url>` and `--disable-auto-update` patch single
   lines with a `config.toml.bak-install` backup. The config is otherwise
   never touched (the auto-update choice stays with the owner; the flags
   exist because the update swap is the prime suspect for the incident).
2. **Autostart** - the `startup install` fallback chain above, unchanged.
3. **Power** - `powercfg /change standby-timeout-ac 0` (AC never sleeps; a
   sleeping machine is the one failure a supervisor cannot heal).
   `--keep-power-plan` skips it.
4. **Supervisor** (the watchdog), best host first:
   - **Windows service `ActivityDaemonWatchdog`** (LocalSystem, needs admin
     once - the non-elevated install hands off via UAC): every 15 s it
     reads the heartbeat. Fresh = leave. Stale with a dead or absent lock
     holder = start. Stale with a live holder whose pid matches the last
     beater = kill and start (wedged run loop). The restart crosses into
     the active console session (`WTSQueryUserToken` +
     `CreateEnvironmentBlock` + `CreateProcessAsUserW`) - the daemon always
     runs in the user session (session 0 cannot capture anything); the
     service only checks and decides. SCM failure recovery
     (`sc failure actions= restart/5000`) restarts the watchdog itself,
     and the binPath carries `service --data-dir <dir>` because LocalSystem
     resolves a different `%APPDATA%`. A failed auto-update relaunch is
     healed by this layer within ~30 s.
   - **Fallback task `ActivityDaemonKeepAlive`**: logon trigger + a PT1M
     repetition running `ensure-run` - the same decision as a one shot, in
     the user session, no admin. Installed automatically when service
     creation is denied (no admin / corporate GPO), degrading full ->
     minimal XML exactly like the autostart chain.

`install status` reports every layer (heartbeat + lock holder, the service
state, the fallback task, the effective autostart mechanism, the patched
config). `install uninstall` removes the service, the task and the
autostart entries; the power change is left in place with the undo command
printed. `install --dry-run` previews the sc commands and both task XMLs
without touching anything. `ensure-run` itself never logs usage
(automatic); `install` / `install uninstall` log one `{install,
install|uninstall}` row each.

## Tray icon and windowless run (issue #30)

The daemon must never show a console window. On Windows, `run` hides its
own console window and shows its presence as a tray icon (bottom-right)
instead:

- **Console hiding** follows `run.hide_console` in config.toml:
  - `auto` (default): hide unless the daemon was started from a visible
    developer terminal (cmd / powershell / pwsh / Windows Terminal keep the
    console for you; explorer, the Startup-folder fallback, the Task
    Scheduler and detached relaunches hide it),
  - `always`: always hide the console window,
  - `never`: never hide it (useful when debugging).
  Non-Windows platforms ignore this setting.
- **Tray icon** (Windows; the process would otherwise be invisible):
  - tooltip reflects version + health: `activity-daemon v0.1.2: active`,
    `activity-daemon v0.1.2: recording` (while the meeting-audio watcher is
    recording), or `activity-daemon v0.1.2: heartbeat stale` (no fresh
    heartbeat file - refreshed every second),
  - right-click menu: **Open dashboard** (the hub web UI at the configured
    `[sync] server_url`, opened in the default browser - no URL to
    remember), **Update now** (runs the same update path as the manual
    `update` command; the daemon restarts gracefully onto the new version)
    and **Quit** (the same graceful shutdown as Ctrl+C - workers join, the
    open window session flushes). Left-click is the same one-click
    dashboard shortcut (a pause switch stays with issue #14); with no
    `server_url` configured both are a logged no-op.
  - the tray never blocks the daemon: if it cannot start (e.g. Explorer is
    not running), the reason is logged and `run` continues without it.

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
| `install` | `install` | `install` (non-dry-run apply; also logged by the elevated retry) |
| `install` | `uninstall` | `install uninstall` (non-dry-run; `install status` does NOT log) |
| `update` | `apply` | manual `update` only - the automatic scheduled check NEVER logs |
| `update` | `check` | manual `update check` only - never the scheduled check |
| `tray` | `open-dashboard` | the tray menu's `Open dashboard` / tray left-click (opens the hub web UI) |
| `tray` | `update-now` | the tray menu's `Update now` (issue #30) |
| `tray` | `quit` | the tray menu's `Quit` (issue #30) |
| `usage` | `view` | the `usage` report itself (honest meta-signal) |

Instrumentation is fire-and-forget: a usage-insert failure prints a warning
and never fails the command you actually ran.

## Docs

- docs/needs.md - owner need profile, source of truth for design decisions
- docs/process.md - dev loop, release flow, post-milestone retro
- docs/data-pipeline.md - v0.2 processing plan (classifier, summaries, export, STT, MCP)
- docs/lessons.md - dev gotchas log
- AGENTS.md - rules for AI collaborators
