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
# unqualified-search registries configured.
FROM docker.io/library/rust:1-slim AS builder
WORKDIR /app
COPY . .
RUN cargo build --release -p server

# Stage 2: minimal runtime.
FROM docker.io/library/debian:stable-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/server /usr/local/bin/activity-server

# Same env contract as docs/sync.md: data under /data (volume), port 8080.
ENV ACTIVITY_PORT=8080 \
    ACTIVITY_DB=/data/activity.db \
    ACTIVITY_DATA=/data
VOLUME /data
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/activity-server"]
