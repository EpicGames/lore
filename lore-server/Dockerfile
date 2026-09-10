FROM rust:slim-trixie AS builder

RUN apt-get update && apt-get install -y \
    build-essential \
    protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY . .

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/target \
    cargo build --release --bin loreserver && \
    cp /build/target/release/loreserver /build/loreserver-bin

FROM debian:trixie-slim

RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/loreserver-bin /usr/local/bin/loreserver
COPY --from=builder /build/lore-server/config/default.toml /etc/lore/config/default.toml

# Docker environment config: local stores at /data. No QUIC certificate is
# configured, so the server generates an ephemeral self-signed cert for the
# public endpoint at startup (zero-config). For a durable deployment, mount a
# real cert into the container and add a [server.quic.certificate] override.
RUN cat <<'EOF' > /etc/lore/config/docker.toml
[immutable_store.local]
path = "/data"

[mutable_store.local]
path = "/data"
EOF

ENV LORE_CONFIG_PATH=/etc/lore/config
ENV LORE_ENV=docker
ENV RUST_LOG=info

RUN mkdir -p /data
VOLUME /data

# QUIC + gRPC: 41337, HTTP: 41339
EXPOSE 41337 41339

ENTRYPOINT ["loreserver"]
