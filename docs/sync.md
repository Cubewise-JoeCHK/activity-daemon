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
  (sync contract v2, issue #40, ADR rev 12): every event is written locally
  before it is forwarded, while a media file is deleted locally once the hub
  has durably accepted both the file (checksum-verified PUT) and its
  referencing index row. No verified upload, a checksum mismatch, or a
  rejected row keeps the file untouched, so an unsynced row can never lose
  its file. The rolling budgets (issue #5/#6) are a backstop for
  hub-unreachable periods only.
- The hub is the **archive**: raw media stays hot for 365 days as monthly
  tar.zst bundles, then moves to the permanent MinIO tier; event rows stay
  in the central SQLite store indefinitely.
- **File first, index row after**: for media, the file is uploaded first -
  acked only after fsync + checksum verification - and only then does the
  index event go out, so the central index never points at a missing file.
  Per forwarder round: capture -> upload -> post batch -> delete.

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

| var               | default                 | meaning                                                        |
|-------------------|-------------------------|----------------------------------------------------------------|
| `ACTIVITY_PORT`   | `8080`                  | port bound on `0.0.0.0`                                        |
| `ACTIVITY_DB`     | `./data/activity.db`    | central SQLite store (WAL)                                     |
| `ACTIVITY_DATA`   | `./data`                | media root; files land under `$ACTIVITY_DATA/media/<kind>/...` |
| `ACTIVITY_TOKEN`  | unset                   | require matching `X-Activity-Token` header on `/ingest`        |
| `ACTIVITY_CONFIG` | `$ACTIVITY_DATA/config.toml` | server config file (`[retention]`, `[retention.minio]`)   |
| `ACTIVITY_MINIO_ACCESS_KEY` | unset            | MinIO permanent tier access key (issue #39; env only, never config) |
| `ACTIVITY_MINIO_SECRET_KEY` | unset            | MinIO permanent tier secret key (issue #39; env only, never config) |

The container image presets `ACTIVITY_PORT=8080`, `ACTIVITY_DB=/data/activity.db`,
`ACTIVITY_DATA=/data` - keep the volume mounted at `/data` and everything
persists. A missing config file means all defaults; an unparsable one stops
the hub (a typo must not silently run with different settings).

## Server config: `[retention]` (issue #38)

Finished months of raw screen frames and meeting audio are bundled into
`bundles/<YYYY-MM>.tar.zst` next to `media/` once they age beyond the hot
window; that month's raw files are deleted only after the bundle verifies.
Events in SQLite, transcripts and summaries are exempt - retention only
ever touches `media/frames/...` and `media/audio/...`. Disabled by default:

```toml
[retention]
enabled = true     # default false - nothing is bundled or deleted until this is set
hot_days = 365     # raw files stay browsable this long (default 365)
bundle = "monthly" # the only cadence today
```

Safety contract:

- The sweeper runs inside the server on a daily tick (one pass at startup,
  then every 24 h). Passes are idempotent and crash-safe: bundles are
  written to a `.part` file and atomically renamed, so a final path only
  ever holds a complete archive; a crash mid-prune is completed by the
  next pass.
- The bundle's first tar member is `_manifest.json` (every file with size
  + sha256). Verification (manifest vs archive listing, total bytes,
  spot-check sample checksums) always runs BEFORE any deletion; a verify
  failure keeps the raw files, logs the reason and retries next tick.
- Deletion is targeted per-file removal - no wildcard or recursive shell
  deletion. A verified bundle also gets a `<bundle>.tar.zst.sha256`
  sidecar (sha256sum format).
- `hot_days` and the current month: a month is eligible once it is fully
  elapsed AND its last day lies strictly beyond the hot window; the
  current month is never touched.

Preview without touching anything:

```sh
activity-server --dry-run   # prints months found, sizes, would-bundle/would-delete
```

`status --json` on a client surfaces the hub state (`GET /api/retention`,
fetched best-effort): hot bytes by kind, bundle count/bytes, months
pending, next scheduled action, per-bundle tier and the MinIO move state
(issue #39).

## Server config: `[retention.minio]` - the permanent tier (issue #39)

Once a month is cold and its bundle is verified, the bundle moves to MinIO
(`blob.joechowhk.internal`, path-style S3) as the **permanent tier**:

```toml
[retention.minio]
enabled  = true
endpoint = "blob.joechowhk.internal"
bucket   = "activity-archive"
prefix   = "bundles" # default; object key = <prefix>/<YYYY-MM>.tar.zst
secure   = false     # TLS; the devbox *.joechowhk.internal vhosts are plain HTTP today
```

Credentials never live in the config file: set `ACTIVITY_MINIO_ACCESS_KEY`
and `ACTIVITY_MINIO_SECRET_KEY` in the hub container's environment. Use a
scoped key (read+write on the bucket only, no delete) so the IAM layer
enforces the never-delete contract.

Contract:

- Layout: `s3://activity-archive/bundles/<YYYY-MM>.tar.zst` plus the
  `.sha256` sidecar alongside. The app performs the move (no MinIO
  lifecycle rule) so the audit trail stays in one place.
- A bundle uploads only after the bundling stage verified it; after the
  upload, the object is fetched back and verified (size + sha256), and
  only then is the hub-local copy deleted (targeted per-file removal).
- Nothing is ever deleted from MinIO by this feature. The only overwrite
  is the idempotent re-upload of the same month's key on retry.
- Upload/verify failures keep the hub-local bundle, record the reason in
  `bundles/minio-state.json`, and retry on the next daily tick (the tick
  cadence is the backoff).
- The move state file is the proof a remote copy exists. If it is
  unreadable, both stages pause and keep everything local (deleting the
  file makes the next pass re-upload every local bundle - overwrites are
  idempotent and verified).
- Late-arriving raw files for an already-moved month stay on the hub and
  are reported; rebundling from MinIO is future scope.

Deployment one-time steps (values live in the infra, not this repo):

```sh
mc mb local/activity-archive          # bucket
mc admin policy attach ...            # scoped key per /opt/minio/policies/ convention
```

## HTTP API

- `GET /health` -> `{"ok": true, "status": "ok", "version": "...", "events_count": n}`
  (`version` feeds the hub updater, issue #32)
- `GET /api/retention` -> retention stats (issues #38/#39, read-only, open
  per the `/api` precedent): `{"enabled", "hot_days", "hot_bytes":
  {"frames", "audio"}, "bundle_count", "bundle_bytes", "months_pending":
  [{"month", "files", "bytes"}], "next_action", "bundles": [{"month",
  "bytes", "tier" ("hot"|"minio")}], "minio": {"enabled", "endpoint",
  "bucket", "prefix", "secure", "moved_months", "moved_bytes",
  "last_move": {"month", "status", "error", "at", "attempts"}}}`.
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
    the ack verifies the durable write; the client deletes its local copy
    only after the referencing index batch is also accepted (sync contract
    v2, issue #40, ADR rev 12).
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
- Media files keep buffering on disk until the hub has durably accepted
  both the bytes (checksum-verified PUT) and the referencing index row:
  deletion happens only then (sync contract v2, issue #40, ADR rev 12) - a
  rejected row keeps its file and retries. During an outage nothing is
  deleted until the rolling budgets bite - at current capture rates roughly
  26 days of frames accumulate before eviction begins; the budgets are the
  outage backstop. Audio retention additionally spares not-yet-synced files
  (ADR rev 5). A media row whose file is missing is skipped and retried
  later - it is never posted and never marked synced.
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
