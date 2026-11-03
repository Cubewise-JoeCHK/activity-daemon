# Central hub server container (ADR revision 6): deployed on the WSL box via podman.
# Build:  podman build -t activity-server .
# Run:    podman run -d -p 8080:8080 -v /srv/activity:/data activity-server

# Stage 1: build only the server binary.
FROM rust:1-slim AS builder
WORKDIR /app
COPY . .
RUN cargo build --release -p server

# Stage 2: minimal runtime.
FROM debian:stable-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/server /usr/local/bin/activity-server

ENV ACTIVITY_SERVER_ADDR=0.0.0.0:8080 \
    ACTIVITY_SERVER_DB=/data/activity.db \
    ACTIVITY_SERVER_MEDIA_DIR=/data/media
VOLUME /data
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/activity-server"]
