# Central hub server container (ADR revisions 5/6): deployed on the WSL box
# via podman. See docs/sync.md for the full runbook.
#
# Build:  podman build -t activity-server .
# Run:    podman run -d --name activity-server -p 0.0.0.0:8080:8080 \
#           -v /srv/activity:/data activity-server
#
# Optional BuildKit speedups (podman --format docker with buildkit or
# `podman build --build-arg`): cache mounts keep the cargo registry and
# target dir between builds, e.g. replace the cargo build step with:
#   RUN --mount=type=cache,target=/usr/local/cargo/registry \
#       --mount=type=cache,sharing=private,target=/app/target \
#       cargo build --release -p server && cp target/release/server /out/
# (the cache-mount variant needs the binary copied OUT of the cached target
# dir before the stage ends, hence the /out/ trick).

# Stage 1: build only the server binary (package name: server).
# Fully-qualified image names: podman on stock Ubuntu has no
# unqualified-search registries configured. The builder is pinned to
# bookworm so the produced binary never demands a newer glibc than the
# runtime base below (issue #32: release binaries are built on
# ubuntu-latest and must run in this image too).
FROM docker.io/library/rust:1-slim-bookworm AS builder
WORKDIR /app
COPY . .
RUN cargo build --release -p server

# Stage 2: minimal runtime - ubuntu:24.04 keeps glibc parity with the
# ubuntu-latest release binary (activity-server-linux-x86_64, consumed by
# the hub updater); a bookworm-built binary is forward-compatible here.
# tzdata: chrono::Local resolves the TZ env var (e.g. TZ=Asia/Hong_Kong
# in the deploy quadlet) against /usr/share/zoneinfo - without it the
# server silently runs UTC and "today" / calendar day boundaries drift
# by the owner's UTC offset.
FROM docker.io/library/ubuntu:24.04
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates tzdata \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/server /usr/local/bin/activity-server

# Same env contract as docs/sync.md: data under /data (volume), port 8080.
ENV ACTIVITY_PORT=8080 \
    ACTIVITY_DB=/data/activity.db \
    ACTIVITY_DATA=/data
VOLUME /data
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/activity-server"]
