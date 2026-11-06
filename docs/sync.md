# Sync: forwarder + central hub (issue #18)

This document is the deployment runbook for the WSL central hub and the
contract the Windows client forwarder (`activity-sync`) follows when pushing
data to it. Design context: ADR revisions 3, 5, 6 and 12 on issue #16 (local
first, file-first upload ordering, client media pass-through with
durability-gated acks, Tailscale transport, no passwords).

## Topology

```
Windows client (plain exe, issue #7)                WSL hub (podman container)
  local SQLite (data/activity.db)  --HTTP-->   server: POST /ingest/events
  media files (data/media/...)     --HTTP-->   server: PUT  /ingest/media/{kind}/{date}/{file}
  both connected over the tailnet (plain HTTP; encryption + identity = Tailscale)
```

- The client is **local-first** for events and a **pass-through** for media
  (sync contract v2, issue #40): every event is written locally before it is
  forwarded, while a media file is deleted locally as soon as the hub's PUT
  ack proves a durable, checksum-verified write. No ack or a checksum
  mismatch keeps the file untouched. The rolling budgets (issue #5/#6) are a
  backstop for hub-unreachable periods only.
- The hub is the **archive**: raw media stays hot for 365 days as monthly
  tar.zst bundles, then moves to the permanent MinIO tier; event rows stay
  in the central SQLite store indefinitely.
- **File first, index row after**: for media, the file is uploaded first -
  acked only after fsync + checksum verification - and only then does the
  index event go out, so the central index never points at a missing file.
  Per forwarder round: capture -> upload -> delete -> post batch.

## Server: build and run (WSL + podman)

Build from a checkout inside the WSL filesystem (not `/mnt/c` - the 9p
bridge is slow for builds):

```sh
# inside WSL Ubuntu-24.04
git clone https://github.com/Cubewise-JoeCHK/activity-daemon.git
cd activity-daemon
podman build -t activity-server .
```

Run (choose one of the two published-port styles):

```sh
mkdir -p /srv/activity/data          # central archive lives here

# style A: explicit port mapping
podman run -d --name activity-server \
  -p 0.0.0.0:8080:8080 \
  -v /srv/activity:/data \
  activity-server

# style B: host networking (container shares WSL's network namespace)
podman run -d --name activity-server \
  --network host \
  -v /srv/activity:/data \
  activity-server
```

Smoke test from WSL:

```sh
curl -s http://127.0.0.1:8080/health
# {"ok":true,"status":"ok","version":"0.1.2","events_count":0}
```

## Updates: the hub updates itself (issue #32)

`activity-server-update` runs on the WSL **host** (a container cannot swap
its own image) and keeps the hub current from GitHub Releases - the same
sha256-verified trust chain as the Windows client (#17), without compiling
on the hub. Release checking, tag comparison and sha256 verification are
shared with the client via the `activity-update` crate.

```
cargo build --release -p activity-server-update   # anywhere, then copy the
# target/release/activity-server-update binary to the WSL box
```

First run **adopts** the manually deployed container (the one from the
build/run steps above): it reads the running version from `/health` (or the
image's `org.opencontainers.image.version` label), and - finding no newer
release - records the container's run arguments and image id as its state
(`~/.local/state/activity-server/state.json`). After that it knows how to
recreate the container and what to roll back to.

```sh
activity-server-update check        # report only: running vs latest release
activity-server-update              # apply now (same as `apply`)
activity-server-update install      # systemd user service: daily checks
activity-server-update status       # service + saved state + running version
activity-server-update rollback     # start the saved rollback image again
activity-server-update uninstall    # remove the service
```

What `apply` does on a newer release, in safety order:

1. Download `activity-server-linux-x86_64` + `.sha256` from the release;
   verify the digest - a mismatch deletes the download and aborts (the old
   container keeps running).
2. Build a **thin image** from the verified binary (no compilation on the
   hub), tagged `activity-server:<tag>` + `activity-server:latest`, labeled
   with the release tag.
3. Smoke test the image: `podman run --rm --network none <image> --version`
   must report the release version before anything is swapped.
4. Swap: `podman stop` (the server drains SIGTERM gracefully) -> `rm` ->
   `run -d` with the args re-derived from the inspect state (ports, binds,
   `ACTIVITY_*` env) plus `--restart=on-failure` - containers deployed
   before #32 gain crash/restart resilience on their first update.
5. Health gate: `/health` must answer within 20 s, else the previous image
   is started again automatically. The swap failure paths likewise restore
   the previous image id.

`install` writes one systemd **user** unit
(`~/.config/systemd/user/activity-server-update.service`, `watch` loop,
`Restart=on-failure`, linger enabled). Trade-offs it prints:
systemd must be enabled inside WSL (`wsl.conf`: `[boot] systemd=true`); the
updater only runs while the WSL VM runs; logs land in
`journalctl --user -u activity-server-update`.

The saved state (run args + rollback image id) also makes the `watch` loop
**self-healing**: if the container ever vanishes mid-swap (e.g. a
`systemctl stop` that lands between the `stop` and `run` steps), the next
tick recreates it from the rollback image.

Manual rollback, without the updater:

```sh
podman stop activity-server && podman rm activity-server
podman run -d --name activity-server -p 0.0.0.0:8080:8080 \
  -v /srv/activity:/data activity-server:v0.1.2     # any earlier tag/id
```

### Auth: mount the tailscaled socket (whois mode)

By default the server runs in **tailnet-trust** mode (a startup log line
says so): it trusts that whoever reaches the port came over the tailnet -
*bind exposure is the tailnet's job*, so do not publish the port beyond the
WSL host. To get device identity automatically, mount the tailscaled socket
into the container; the server detects it at startup and resolves every
ingest request's source IP to a device name via `tailscaled whois`,
overriding the `device` field in request bodies:

```sh
podman run -d --name activity-server \
  -p 0.0.0.0:8080:8080 \
  -v /srv/activity:/data \
  -v /run/tailscale/tailscaled.sock:/var/run/tailscale/tailscaled.sock \
  activity-server
```

(The socket path on the host is `/run/tailscale/tailscaled.sock` on
Ubuntu's packaged tailscale; the container looks for it at
`/var/run/tailscale/tailscaled.sock`.)

### Shared-token hardening (optional)

Set `ACTIVITY_TOKEN` to require `X-Activity-Token: <token>` on every
`/ingest` request (`401` otherwise). Composable with whois mode; `/health`
stays open for probes. If the token is ever set, the client's `config.toml`
grows the same value and the forwarder sends the header (wiring in #7).

## Environment variables (ACTIVITY_*)

| var              | default               | meaning                                                        |
|------------------|-----------------------|----------------------------------------------------------------|
| `ACTIVITY_PORT`  | `8080`                | port bound on `0.0.0.0`                                        |
| `ACTIVITY_DB`    | `./data/activity.db`  | central SQLite store (WAL)                                     |
| `ACTIVITY_DATA`  | `./data`              | media root; files land under `$ACTIVITY_DATA/media/<kind>/...` |
| `ACTIVITY_TOKEN` | unset                 | require matching `X-Activity-Token` header on `/ingest`        |

The container image presets `ACTIVITY_PORT=8080`, `ACTIVITY_DB=/data/activity.db`,
`ACTIVITY_DATA=/data` - keep the volume mounted at `/data` and everything
persists.

## HTTP API

- `GET /health` -> `{"ok": true, "status": "ok", "version": "...", "events_count": n}`
  (`version` feeds the hub updater, issue #32)
- `POST /ingest/events`
  - body: `{"device": "...", "events": [{"bucket", "timestamp" (RFC3339 with
    offset), "duration_s", "data"} ...]}`
  - per-event validation (core's `EventEnvelope::validate`); append-only
    insert; inserted rows are marked synced on the hub (they arrived by
    definition).
  - response: `{"accepted": n, "rejected": [{"index", "reason"}]}` - 200
    when anything was accepted, 400 when the whole batch was malformed
    (bad JSON, empty device, empty batch, or every event rejected).
- `PUT /ingest/media/{kind}/{YYYY-MM-DD}/{filename}`
  - raw body streamed to `<media_root>/<kind>/<YYYY>/<MM>/<DD>/<filename>`;
    optional `X-SHA256` (lowercase hex) verified after write; mismatch
    deletes the file and returns 400. The file is fsynced before the ack -
    the ack is deletion clearance for the client (sync contract v2,
    issue #40).
  - response: `{"stored": "media/<kind>/<Y>/<M>/<D>/<file>", "bytes": n,
    "sha256": "<hex of the stored bytes>"}`.
  - path traversal is rejected in every segment (kind/date/filename are
    restricted to ASCII alphanumerics plus `.`, `-`, `_`; date must be
    `YYYY-MM-DD`; `.` and `..` never pass).

## Client configuration (issue #7 wiring)

`config.toml` on the Windows client:

```toml
data_dir = "C:/activity"        # local archive root (activity.db + media/)

[sync]
server_url = "http://activity-hub:8080"   # Tailscale MagicDNS name or 100.x.y.z IP
device_name = "windows-client"            # identity in the central store
auto_update = true
```

`activity-sync` derives its settings from that section
(`SyncSettings::from_config`): `server_url`, `device_name`, plus defaults
`interval_s = 30` (forwarder round cadence) and `batch_size = 500`. The
daemon binary (#7) runs `activity_sync::run(&settings, &db, &data_dir,
&shutdown)` on a worker thread and flips the `AtomicBool` on shutdown.

Any host on the tailnet can also drive the API by hand:

```sh
curl -s http://activity-hub:8080/ingest/events \
  -H 'content-type: application/json' \
  -d '{"device":"manual","events":[{"bucket":"note","timestamp":"2026-11-04T12:00:00+08:00","duration_s":0,"data":{"text":"hello"}}]}'
```

## What happens when the server is down

- The forwarder's POST/PUT fail; it backs off exponentially with jitter
  (1 s, 2 s, 4 s ... capped at 5 minutes) and retries. Local collection
  (watchers) never notices - they only write locally.
- Events keep buffering in the local SQLite `events` table with
  `synced = 0` (bounded batches of 500 per round; a full batch drains
  immediately without waiting for the next interval).
- Media files keep buffering on disk until the hub acks them: deletion
  happens only on a durable, checksum-verified ack (sync contract v2,
  issue #40). During an outage nothing is deleted until the rolling budgets
  bite - at current capture rates roughly 26 days of frames accumulate
  before eviction begins; the budgets are the outage backstop. Audio
  retention additionally spares not-yet-synced files (ADR rev 5). A media
  row whose file is missing is skipped and retried later - it is never
  posted and never marked synced.
- Rejected events (server-side 4xx with reasons) stay unsynced and are
  logged each round - nothing is silently dropped; a poison row shows up in
  the client log and can be fixed or removed manually.

## Build notes

- Server image: multi-stage `Containerfile` (pinned rust:1-slim-bookworm
  builder -> ubuntu:24.04 runtime), `EXPOSE 8080`, data volume `/data`.
  The runtime base keeps glibc parity with the `ubuntu-latest` release
  binary the hub updater consumes (issue #32); the pinned builder
  guarantees a source-built binary never demands a newer glibc than the
  runtime.
- The server handles SIGINT (Ctrl+C) and SIGTERM (`podman stop`) with the
  same graceful drain, so an update swap never cuts in-flight requests.
- `activity-sync` uses `ureq` (small blocking client, rustls TLS, JSON
  support): the forwarder is a plain loop, not a service, so no async
  runtime is pulled in on the client.
- CI (windows + ubuntu) compiles and tests both crates; the container build
  is validated on the WSL box per issue #18.
