FROM rust:slim-trixie AS builder

RUN apt-get update && apt-get install -y \
    build-essential \
    protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY . .

# .cargo/config.toml tunes aarch64-unknown-linux-gnu for Epic's own Graviton3+
# deployment with -C target-cpu=neoverse-512tvb. That emits instructions which
# fault on older arm64 parts, and because this build passes no --target, the
# same flags reach the proc macros cargo then executes on the build host. A
# published image has to run anywhere armv8-a runs, so drop the tuning.
#
# The flags kept below are the ones that block would otherwise supply: a
# [target.*] rustflags table fully replaces the [build] list rather than
# merging with it, and the two --cfg values are load-bearing — the code does
# not compile without them.
ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS="--cfg tokio_unstable -C force-unwind-tables=yes -C force-frame-pointers=yes --cfg uuid_unstable"

# Epic's own builds keep full debug symbols in the server binary. An image
# pulled over the network does not want the DWARF; drop it but keep the symbol
# table so backtraces still name their frames.
ENV CARGO_PROFILE_RELEASE_STRIP=debuginfo

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
