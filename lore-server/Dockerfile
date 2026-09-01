FROM rust:slim-trixie AS builder

RUN apt-get update && apt-get install -y \
    build-essential \
    protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY . .

# Which arm64 microarchitecture to tune for. Empty is baseline armv8-a, which
# runs anywhere — Apple Silicon, Ampere, any non-Graviton host — and is what
# the default image ships. The publish workflow passes neoverse-512tvb for the
# separate -graviton tag, matching how Lore is built and deployed on
# Graviton3+. Ignored on amd64, which is baseline either way.
ARG TARGETARCH
ARG ARM64_TARGET_CPU=""

# The flags are assembled here rather than left to .cargo/config.toml, whose
# [target.aarch64-unknown-linux-gnu] table pins Graviton3+ unconditionally.
# Overriding that needs RUSTFLAGS specifically, not
# CARGO_TARGET_<triple>_RUSTFLAGS: the latter is only another source for the
# same config key and cargo *joins* config arrays, so the config's
# -C target-cpu would survive and a baseline build would still SIGILL off
# Graviton. RUSTFLAGS replaces [build] and [target.*] outright, hence the
# repetition. The --cfg values are load-bearing, the clippy lints are not.
#
# Not `--release`: that profile is Cargo.toml's local-development build, with
# debug-assertions and so overflow-checks left on. release-lto is the real one.
# It asks for full DWARF, which --strip-debug drops again, keeping the symbol
# table so backtraces still name their frames.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/target \
    set -eu; \
    RUSTFLAGS="--cfg tokio_unstable --cfg uuid_unstable -C force-unwind-tables=yes -C force-frame-pointers=yes"; \
    if [ "${TARGETARCH}" = "arm64" ] && [ -n "${ARM64_TARGET_CPU}" ]; then \
      RUSTFLAGS="${RUSTFLAGS} -C target-cpu=${ARM64_TARGET_CPU}"; \
    fi; \
    export RUSTFLAGS; \
    echo "building with RUSTFLAGS=${RUSTFLAGS}"; \
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

# QUIC + gRPC: 41337, HTTP: 41339
EXPOSE 41337 41339

ENTRYPOINT ["loreserver"]
