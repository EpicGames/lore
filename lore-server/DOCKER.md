# Running loreserver in Docker

A basic Docker image for running loreserver with local filesystem storage. No authorization,
telemetry integration, or replication is configured.

## Prerequisites

- Docker with BuildKit support
- On Apple Silicon (M-series Macs), builds must target `linux/amd64` due to Graviton-specific
  compiler flags in `.cargo/config.toml` for `aarch64-unknown-linux-gnu`

`lore-server/Dockerfile` is the one to build from source, and is what the rest of this section
describes. `lore-server/Dockerfile.release` packages a binary already published as a release asset
and compiles nothing; it is used only by the publish workflow, and needs that asset unpacked into
`dist/<arch>/` beside it.

## Building

From the repository root:

```sh
docker build --platform linux/amd64 -f lore-server/Dockerfile -t loreserver .
```

The build compiles the `loreserver` binary and generates self-signed TLS certificates for QUIC
using `scripts/server/make-certs.sh`.

## Published images

The publish workflow packages the `loreserver` binaries from a published release — it compiles
nothing — and pushes two variants to `ghcr.io/epicgames/lore/loreserver`:

| Tag | Platforms |
| --- | --- |
| `X.Y.Z`, `X.Y`, `latest` | `linux/amd64` only |
| `X.Y.Z-graviton`, `X.Y-graviton`, `latest-graviton` | `linux/arm64` tuned for Graviton3+ |

**On Graviton3 or newer**, pull `-graviton` for a native arm64 image. **Anywhere else**, pull the
unsuffixed tag.

`latest` and `latest-graviton` move only when the release being packaged is the newest stable one,
so backfilling an old release or hotfixing an older line does not drag them backwards. A prerelease
never takes them.

Releases ship no baseline `armv8-a` Linux binary. The only `aarch64-unknown-linux-gnu` build is
tuned for Graviton3+, and the `aarch64-apple-darwin` build is a macOS Mach-O executable, which
cannot go into a Linux image at all. So arm64 is offered only under the suffixed tag, where the
name says what it is: nothing in the pull path consults CPU features, so an unsuffixed tag
carrying that binary would hand it to Apple Silicon and Ampere hosts, which fault on it with
`SIGILL`.

The unsuffixed variant is `linux/amd64` alone, published as a plain manifest rather than a
single-entry index. That distinction matters: an index listing only `linux/amd64` makes an arm64
host fail the pull with `no matching manifest for linux/arm64/v8`, whereas a plain manifest runs
under emulation with a warning. It is the reason that variant carries no build provenance
attestation — buildx attaches provenance as a second manifest, and two manifests make an index.
Both variants are signed regardless.

On Apple Silicon, run the release's `aarch64-apple-darwin` binary directly rather than reaching for
a container. When releases start shipping a baseline `armv8-a` Linux binary, the unsuffixed variant
can carry `linux/arm64` too.

The `default.toml` baked in comes from the release tag rather than from the branch the workflow ran
from, since config keys move between releases and an older binary can fail to start on a newer
file. Only the packaging — the Dockerfile — comes from the workflow's own ref.

Every tag is signed keylessly with cosign, and the run summary prints the `cosign verify`
invocation for the digest it published, along with the release assets packaged and their SHA-256 —
the releases carry no checksums of their own, so that is what ties an image back to exact bytes. A
`sha-<commit>-<version>` tag (and `-graviton`) appears alongside each release, on the same digest:
the signature is made against that digest before any release tag is pointed at it, so no release
tag is ever briefly unsigned. It stays afterwards as a record of which commit published which
image. The version is part of the name because a backfill runs from the default branch, so the
commit alone would not tell two backfills apart.

## Running

```sh
docker run -p 41337:41337/tcp -p 41337:41337/udp -p 41339:41339 loreserver
```

Both TCP and UDP mappings are required on port 41337 because gRPC uses TCP and QUIC uses UDP.

No QUIC certificate is baked into the image, so the server generates an ephemeral self-signed one
at startup and clients have to be told to trust it. For anything durable, mount a real certificate
and point `[server.quic.certificate]` at it.

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
- `docker.toml` — overrides the immutable and mutable store paths to `/data`. Loaded as the `docker` environment layer (`LORE_ENV=docker`). It configures no QUIC certificate, which is what leaves the server generating an ephemeral self-signed one.

Settings can be overridden via environment variables with the `LORE__` prefix and `__` as the
separator. For example:

```sh
docker run -e LORE__SERVER__HTTP__PORT=8080 -p 8080:8080 -p 41337:41337/tcp -p 41337:41337/udp loreserver
```
