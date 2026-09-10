FROM rust:slim-trixie AS builder

RUN apt-get update && apt-get install -y \
    build-essential \
    protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY . .

# Which arm64 microarchitecture to tune for. Empty is baseline armv8-a, which
# runs anywhere; the -graviton image passes neoverse-512tvb. Ignored on amd64.
ARG TARGETARCH
ARG ARM64_TARGET_CPU=""

# RUSTFLAGS is restated in full because .cargo/config.toml pins Graviton3+ for
# this target and cargo *joins* config arrays: only RUSTFLAGS replaces [build]
# and [target.*] outright, so anything less leaves a baseline build SIGILLing.
# LORE_ARM64_TARGET_CPU carries the same choice to lore-base's cc build of
# rpmalloc, which RUSTFLAGS cannot reach.
#
# Not `--release`: that profile is local development, debug-assertions on.
# release-lto asks for `debug = 2`, which a fat-LTO link gets OOM-killed for on
# a 16 GB runner; `strip --strip-debug` below would discard it regardless.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/target \
    set -eu; \
    RUSTFLAGS="--cfg tokio_unstable --cfg uuid_unstable -C force-unwind-tables=yes -C force-frame-pointers=yes"; \
    RUSTFLAGS="${RUSTFLAGS} -C debuginfo=0"; \
    if [ "${TARGETARCH}" = "arm64" ] && [ -n "${ARM64_TARGET_CPU}" ]; then \
      RUSTFLAGS="${RUSTFLAGS} -C target-cpu=${ARM64_TARGET_CPU}"; \
    fi; \
    export RUSTFLAGS; \
    export LORE_ARM64_TARGET_CPU="${ARM64_TARGET_CPU}"; \
    echo "building with RUSTFLAGS=${RUSTFLAGS} LORE_ARM64_TARGET_CPU=${ARM64_TARGET_CPU}"; \
    cargo build --profile release-lto --bin loreserver; \
    cp /build/target/release-lto/loreserver /build/loreserver-bin; \
    strip --strip-debug /build/loreserver-bin

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

# 41337 carries gRPC over TCP and QUIC over UDP; both are needed.
EXPOSE 41337/tcp 41337/udp 41339/tcp

ENTRYPOINT ["loreserver"]
