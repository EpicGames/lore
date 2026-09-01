FROM rust:slim-trixie AS builder

RUN apt-get update && apt-get install -y \
    build-essential \
    protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY . .

# .cargo/config.toml tunes aarch64 for Graviton3+ with -C
# target-cpu=neoverse-512tvb, which SIGILLs on older arm64 parts. Dropping it
# needs RUSTFLAGS, not CARGO_TARGET_<triple>_RUSTFLAGS: that one only joins the
# config arrays, RUSTFLAGS replaces [build] and [target.*] outright. Hence the
# repetition; the --cfg values are load-bearing, the clippy lints are not.
ENV RUSTFLAGS="--cfg tokio_unstable --cfg uuid_unstable -C force-unwind-tables=yes -C force-frame-pointers=yes"

# Not `--release`: that profile is Cargo.toml's local-development build, with
# debug-assertions and so overflow-checks left on. release-lto is the real one.
# It asks for full DWARF, which --strip-debug drops again, keeping the symbol
# table so backtraces still name their frames.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/target \
    cargo build --profile release-lto --bin loreserver && \
    cp /build/target/release-lto/loreserver /build/loreserver-bin && \
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
