# Sync: forwarder + central hub (issue #18)

This document is the deployment runbook for the WSL central hub and the
contract the Windows client forwarder (`activity-sync`) follows when pushing
data to it. Design context: ADR revisions 3, 5 and 6 on issue #16 (local
first, file-first upload ordering, central keep-forever archive, Tailscale
transport, no passwords).

## Topology

```
Windows client (plain exe, issue #7)                WSL hub (podman container)
  local SQLite (data/activity.db)  --HTTP-->   server: POST /ingest/events
  media files (data/media/...)     --HTTP-->   server: PUT  /ingest/media/{kind}/{date}/{file}
  both connected over the tailnet (plain HTTP; encryption + identity = Tailscale)
```

- The client is **local-first**: every event and every media file is written
  locally before anything is forwarded. Nothing in this pipeline deletes
  client data (rolling retention on the client - issue #5/#6 - only prunes
  files already synced).
- The hub is the **archive**: server-side retention defaults to
  keep-forever (configurable cap if disk ever demands it).
- **File first, index row after**: for media, the file is uploaded first and
  only then does the index event go out, so the central index never points
  at a missing file.

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
# {"ok":true,"status":"ok","events_count":0}
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

- `GET /health` -> `{"ok": true, "status": "ok", "events_count": n}`
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
    deletes the file and returns 400.
  - response: `{"stored": "media/<kind>/<Y>/<M>/<D>/<file>", "bytes": n}`.
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
- Media files keep buffering on disk. **Rolling retention never deletes
  unsynced files** (ADR rev 5): the forwarder only ever marks a media row
  synced after BOTH the file upload and the event batch succeeded, and the
  client retention rules prune synced files only. A media row whose file is
  missing is skipped and retried later - it is never posted and never
  marked synced.
- Rejected events (server-side 4xx with reasons) stay unsynced and are
  logged each round - nothing is silently dropped; a poison row shows up in
  the client log and can be fixed or removed manually.

## Build notes

- Server image: multi-stage `Containerfile` (rust:1-slim builder ->
  debian:stable-slim runtime), `EXPOSE 8080`, data volume `/data`.
- `activity-sync` uses `ureq` (small blocking client, rustls TLS, JSON
  support): the forwarder is a plain loop, not a service, so no async
  runtime is pulled in on the client.
- CI (windows + ubuntu) compiles and tests both crates; the container build
  is validated on the WSL box per issue #18.
