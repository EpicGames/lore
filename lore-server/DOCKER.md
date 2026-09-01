# Running loreserver in Docker

A basic Docker image for running loreserver with local filesystem storage. No authorization,
telemetry integration, or replication is configured.

## Prerequisites

- Docker with BuildKit support

Both `linux/amd64` and `linux/arm64` build. `.cargo/config.toml` pins `aarch64-unknown-linux-gnu`
to Graviton3+ via `-C target-cpu=neoverse-512tvb`, which faults on older arm64 parts, so the
Dockerfile assembles `RUSTFLAGS` itself and leaves that tuning off by default. The arm64 image
therefore runs on any armv8-a host, Apple Silicon included.

## Building

From the repository root:

```sh
docker build -f lore-server/Dockerfile -t loreserver .
```

Pass `--platform linux/amd64` or `--platform linux/arm64` to cross-build; expect it to be slow,
since a release Rust build under emulation is far slower than a native one.

To tune arm64 for Graviton3 and newer, as Lore is deployed, pass the microarchitecture. The
resulting binary will not run on older arm64 hardware:

```sh
docker build -f lore-server/Dockerfile --build-arg ARM64_TARGET_CPU=neoverse-512tvb -t loreserver .
```

## Published images

The publish workflow ships both variants to `ghcr.io/epicgames/lore/loreserver`:

| Tag | arm64 build |
| --- | --- |
| `X.Y.Z`, `X.Y`, `latest` | baseline `armv8-a` — runs on any arm64 host |
| `X.Y.Z-graviton`, `latest-graviton` | tuned for Graviton3+ — faults on older arm64 |

`linux/amd64` is baseline in both, and is the same image in each manifest list.

The build compiles the `loreserver` binary and generates self-signed TLS certificates for QUIC
using `scripts/server/make-certs.sh`.

## Running

```sh
docker run -p 41337:41337/tcp -p 41337:41337/udp -p 41339:41339 loreserver
```

Both TCP and UDP mappings are required on port 41337 because gRPC uses TCP and QUIC uses UDP.

### Persisting data

By default, store data is written to `/data` inside the container and is lost when the container
stops. Mount a host directory to persist it across restarts:

```sh
docker run \
  -p 41337:41337/tcp \
  -p 41337:41337/udp \
  -p 41339:41339 \
  -v /path/to/local/data:/data \
  loreserver
```

## Ports

| Port  | Protocol | Service        |
|-------|----------|----------------|
| 41337 | TCP      | gRPC           |
| 41337 | UDP      | QUIC           |
| 41339 | TCP      | HTTP           |

## Configuration

The image stores config files in `/etc/lore/config/` (`LORE_CONFIG_PATH`):

- `default.toml` — copied from `lore-server/config/default.toml` at image build time. Loaded as the on-disk default layer on top of the compiled-in defaults, so you can mount a custom `default.toml` to override compiled-in values without rebuilding the image.
- `docker.toml` — overrides store paths to `/data` and configures QUIC TLS certificates. Loaded as the `docker` environment layer (`LORE_ENV=docker`).

Settings can be overridden via environment variables with the `LORE__` prefix and `__` as the
separator. For example:

```sh
docker run -e LORE__SERVER__HTTP__PORT=8080 -p 8080:8080 -p 41337:41337/tcp -p 41337:41337/udp loreserver
```
