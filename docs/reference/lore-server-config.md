# Lore Server configuration reference

## Synopsis

```bash
loreserver [--config <DIR>] [--env <ENV>]
```

The Lore Server binary (`loreserver`) reads its settings from three layered sources: the built-in defaults baked into the binary at compile time, optional TOML files layered over them, and `LORE__`-prefixed environment variables. All configuration is optional — run `loreserver` with no arguments and it starts a throwaway single-node server from the built-in defaults. This page catalogs every CLI flag, config-file layer, and settings field, including the AWS, DynamoDB, Consul, and hook plugin backends, and the default each falls back to.

This page documents the configuration surface only. To stand up a server step by step, see [Deploy a local Lore Server](../how-to/deploy-local-lore-server.md). For the guided first-run flow, see the [Quickstart](../tutorials/quickstart.md).

## CLI flags

| Flag | Argument | Environment fallback | Default | Description |
| --- | --- | --- | --- | --- |
| `--config` | `<DIR>` | `LORE_CONFIG_PATH` | `lore-server/config` (relative to the working directory) | Directory of TOML config files layered over the built-in defaults. |
| `--env` | `<ENV>` | `LORE_ENV` | `local` | Environment name selecting the `<environment>.toml` override to load. |
| `--help`, `-h` | none | none | none | Print help and exit. |
| `--version`, `-V` | none | none | none | Print the version and exit. |

### `--config`

Sets the directory the server loads optional override files from (see [Config-file layering](#config-file-layering)). When neither `--config` nor `LORE_CONFIG_PATH` is set, the directory defaults to the relative path `lore-server/config` — so launched from the repository root the server picks up `lore-server/config/local.toml`, while launched from a directory without a `lore-server/config` folder it runs on the built-in defaults alone. Each file in the directory is optional. The flag falls back to its environment variable, so `LORE_CONFIG_PATH=/etc/lore/config` is equivalent to `--config /etc/lore/config`.

### `--env`

Selects which `<environment>.toml` file the server layers over the defaults. Defaults to `local` when neither `--env` nor `LORE_ENV` is set, so a config directory with a `local.toml` overlay is picked up without any extra flag.

## Config-file layering

The server layers the following files from the config directory over the built-in defaults. The config directory is whatever `--config` / `LORE_CONFIG_PATH` names, or the relative `lore-server/config` path when neither is set. Each file is optional; a missing file — or a missing config directory — is silently skipped. Sources merge field by field rather than replacing one another wholesale: each later source overrides only the individual fields it sets and leaves every other field intact. When two files set **different** fields, both apply; when they set the **same** field, the later source wins.

| Order | Source | When it loads |
| --- | --- | --- |
| 1 | Built-in `default.toml` | Always. Baked into the binary at compile time. |
| 2 | `<environment>.toml` | When present in the config directory. The environment comes from `--env` / `LORE_ENV` (default `local`). |
| 3 | `local.toml` | When present in the config directory. If it exists, `local.toml` is applied as the last file layer, and will override every file above it. |
| 4 | `LORE__`-prefixed environment variables | Always. Overrides every file layer (see [Environment-variable overrides](#environment-variable-overrides)). |

For example, with `--env dev`, the server reads `default.toml` (baked in), then `dev.toml`, then `local.toml`, then applies environment variables.
For the complete loading sequence — including advanced region-scoped overlays and other power-user layers — see the inline documentation in the `loreserver` configuration source.

> [!NOTE]
> Field-level merging applies to TOML tables. **Array-valued fields** — for example `topology.fixed.peers` — are replaced wholesale by the last source that sets them; values are not concatenated across layers. The same holds for `LORE__` environment-variable overrides.

<!-- -->

> [!IMPORTANT]
> When `--config` and `LORE_CONFIG_PATH` are both unset, the config directory defaults to the relative path `lore-server/config`, resolved against the current working directory. The server still layers any optional files it finds there — so running from the repository root loads `lore-server/config/local.toml`, while running from a directory that has no `lore-server/config` folder applies only the built-in defaults (see [Zero-config defaults](#zero-config-defaults)). To load a specific overlay regardless of working directory, point `--config` at the directory that holds it.

## Environment-variable overrides

Any scalar field can be overridden with an environment variable prefixed `LORE__`. Nested keys use a double-underscore (`__`) separator that maps to the TOML table path; single underscores inside a field name are preserved (so `presigned_url_hmac_key` stays intact). The entire variable name is matched case-insensitively — both the `LORE__` prefix and the segments after it may be in any case.

| Field | Environment variable |
| --- | --- |
| `topology.provider` | `LORE__TOPOLOGY__PROVIDER` |
| `server.http.presigned_url_hmac_key` | `LORE__SERVER__HTTP__PRESIGNED_URL_HMAC_KEY` |
| `immutable_store.local.path` | `LORE__IMMUTABLE_STORE__LOCAL__PATH` |

Environment-variable overrides apply last, after every file layer, so they win over anything in the config directory.

> [!NOTE]
> Only scalar fields can be set this way. Array-valued fields — such as the `peers` list under `[topology.fixed]` — cannot be overridden through a single environment variable; configure those in a file layer instead.

## Zero-config defaults

With no config files loaded, the server runs as a self-contained, single-node instance, substituting ephemeral local artifacts for what a production deployment would configure. It logs each substitution so the choice is visible.

- **TLS certificate.** When the public-facing QUIC endpoint has no certificate, the server writes a self-signed certificate for `localhost`, `127.0.0.1`, and `::1` to `<temp>/lore-server/<endpoint>-cert.pem` (and `-key.pem`). It's untrusted, regenerated on every restart, and for local development only.
- **Local store path.** When a local store has no `path`, it uses `<temp>/lore-server`. Because that path is fixed, a later run reopens the same directory and reuses whatever the previous run left.
- **Presigned URL feature.** When `presigned_url_hmac_key` is absent, the feature starts disabled.

> [!NOTE]
> `<temp>` is the OS temporary directory (`$TMPDIR` or `/tmp` on Linux; a per-user `/var/folders/…` path on macOS). The server always uses the same fixed subdirectory, `<temp>/lore-server`, so the paths above are stable across runs.

<!-- -->

> [!WARNING]
> The zero-config store lives under `<temp>/lore-server`, which the OS can clear on reboot — taking every fragment and branch pointer with it. Set an explicit `path` on the local stores for any data that must survive a reboot.

## Server and endpoint settings

The `[server]` table and its sub-tables configure the network endpoints and graceful-shutdown behavior.

| Field | Default | Description |
| --- | --- | --- |
| `server.connection_close_timeout_seconds` | `5` | Seconds to wait for open connections to close after a shutdown signal. |
| `server.runtime_shutdown_timeout_seconds` | `25` | Seconds to wait for the async runtime to shut down after connections close. Accepts the alias `shutdown_delay_seconds`. |

### QUIC endpoints

`[server.quic]` is the public-facing QUIC endpoint that clients push to and clone from. `[server.quic_internal]` is the internal replication endpoint, opt-in per environment and requiring mutual TLS. Both use the same field set.

| Field | Default (`server.quic`) | Description |
| --- | --- | --- |
| `enabled` | `true` (`quic`), `false` (`quic_internal`) | Whether to start this QUIC endpoint. |
| `host` | `0.0.0.0` | Bind address. |
| `port` | `41337` (`quic`), `41340` (`quic_internal`) | Listen port. |
| `verify_client_certs` | `false` (`quic`), `true` (`quic_internal`) | Require client certificates (mutual TLS); `false` accepts unverified clients. The compile-time fallback is `true`, but the shipped `default.toml` sets the public endpoint to `false` so token (JWT) auth carries verification. `quic_internal` omits the key and so keeps the `true` default. |
| `idle_timeout` | `30000` | Connection idle timeout in milliseconds. |
| `keep_alive` | `500` | Keep-alive interval in milliseconds. |
| `max_bidi_streams` | `8` | Maximum concurrent bidirectional streams per connection. |
| `num_listeners` | `10` | Number of listener tasks. |
| `transport_bits_per_second` | `1073741824` (`quic`), `10737418240` (`quic_internal`) | Transport bandwidth estimate in bits per second. |
| `transport_rtt` | `100` | Expected round-trip time in milliseconds. |
| `handler_timeout_seconds` | `50` | Per-request handler timeout. Guards against handlers that hang indefinitely. |
| `connection_message_limit` | none | Maximum in-flight messages per connection. When unset, the server falls back to `500` for the public endpoint and `50000` for the internal endpoint. |

#### Certificate block

Each QUIC endpoint takes an optional `[server.quic.certificate]` (or `[server.quic_internal.certificate]`) block. When omitted on the public endpoint, the server generates an ephemeral certificate (see [Zero-config defaults](#zero-config-defaults)). The block as a whole is optional, but when it is present `cert_file` and `pkey_file` are both required — only `cert_chain` is individually optional.

| Field | Default | Description |
| --- | --- | --- |
| `cert_file` | none | Path to the PEM-encoded server certificate. |
| `pkey_file` | none | Path to the PEM-encoded private key. |
| `cert_chain` | none | Path to PEM-encoded CA certificate(s) used to verify client certificates (and also appended to the chain the server presents). Required on any endpoint with `verify_client_certs = true`; the internal endpoint enforces this at startup. |

### HTTP endpoint

`[server.http]` configures the HTTP endpoint that serves the health check and, when enabled, presigned URLs.

| Field | Default | Description |
| --- | --- | --- |
| `enabled` | `true` | Whether to start the HTTP endpoint. |
| `host` | `0.0.0.0` | Bind address. |
| `port` | `41339` | Listen port. |
| `max_file_size` | `10485760` | Maximum upload size in bytes (10 MiB). |
| `request_timeout_seconds` | `300` | Overall request timeout. |
| `request_body_timeout_seconds` | `3600` | Request-body read timeout. |
| `available_interval_seconds` | `30` | Interval between store-availability probes. |
| `available_timeout_seconds` | `5` | Timeout for each store-availability probe. |
| `store_health_check` | `false` | Whether the health check also probes the store. |
| `presigned_url_hmac_key` | none | Optional hex-encoded HMAC key that enables the presigned URL feature. See below. |
| `presigned_url_min_ttl_seconds` | `1` | Minimum lifetime a presigned URL may request. |
| `presigned_url_default_ttl_seconds` | `3600` | Default presigned URL lifetime. |
| `presigned_url_max_ttl_seconds` | `86400` | Maximum presigned URL lifetime. |

#### `presigned_url_hmac_key`

This field is optional. When it's absent, the server starts with the presigned URL feature disabled and logs `Presigned URL feature disabled (presigned_url_hmac_key not configured)`. When it's set, the value must be valid hexadecimal that decodes to at least 32 bytes — generate one with `openssl rand -hex 32`. An invalid or too-short key stops the server from starting. Set this only for deployments that hand out presigned URLs, and use a fresh key per deployment.

### gRPC endpoints

`[server.grpc]` is the public gRPC API (HTTP/2 over TCP) serving the admin, storage, revision, repository, environment, lock, and notification services. It runs whenever the server is in normal (non-maintenance) mode. `[server.grpc_internal]` is the opt-in server-to-server gRPC internal endpoint; it is disabled by default and requires mutual TLS. Both tables share the same field set.

> [!NOTE]
> `[server.grpc]`'s default port `41337` is the same number as `[server.quic]`, but the two do not conflict: gRPC listens on TCP and QUIC on UDP.

| Field | Default (`server.grpc`) | Description |
| --- | --- | --- |
| `host` | `0.0.0.0` | Bind address. |
| `port` | `41337` (`grpc`), `41340` (`grpc_internal`) | Listen port. |
| `request_handler_timeout_seconds` | `50` | Per-request handler timeout in seconds. Keep below any upstream load-balancer timeout. |
| `http2_keepalive_interval_seconds` | none | HTTP/2 keep-alive PING interval in seconds. Unset by default. |
| `http2_keepalive_timeout_seconds` | none | HTTP/2 keep-alive PING timeout in seconds. Unset by default. |
| `certificate` | none | Optional `[server.grpc.certificate]` / `[server.grpc_internal.certificate]` block — same fields as the [Certificate block](#certificate-block). When present, the endpoint serves TLS. |

`[server.grpc_internal]` additionally honors two fields that `[server.grpc]` ignores:

| Field | Default | Description |
| --- | --- | --- |
| `enabled` | `false` | Whether to start the replication endpoint. Set `true` to opt in. |
| `verify_client_certs` | `true` | Require client certificates (mutual TLS). The endpoint refuses to start unless this is `true` with a full certificate triple (`cert_file` + `pkey_file` + `cert_chain`), or explicitly set to `false` to accept unverified clients. |

### gRPC public-service tuning

`[server.grpc_public_services]` applies per-service tuning to the public gRPC endpoint. Only the lock service is currently configurable.

| Field | Default | Description |
| --- | --- | --- |
| `lock_service.max_encoding_message_size` | `16777216` (16 MiB) | Maximum encoded gRPC response size, in bytes, for the lock service. When unset, the gRPC framework default applies. |

### Authentication

`[server.auth]` configures JWT verification for the gRPC API. When `[server.auth]` (or its `[server.auth.jwk]` sub-table) is absent — as in every shipped config — JWT verification is disabled and the gRPC services accept unauthenticated requests.

| Field | Default | Description |
| --- | --- | --- |
| `jwt_issuer` | none | Expected JWT `iss` claim. When set, tokens with a different issuer are rejected; when unset, issuer validation is skipped. |
| `jwt_audience` | none | Array of accepted JWT `aud` values. A token's audience must match one entry; when unset, audience validation is skipped. |
| `jwk` | none | The `[server.auth.jwk]` sub-table below. Its presence enables JWT verification. |

`[server.auth.jwk]`:

| Field | Default | Description |
| --- | --- | --- |
| `endpoint` | none (required) | URL of the JWKS (JSON Web Key Set) endpoint. The server fetches and caches signing keys from it at startup and re-fetches on an unknown key ID. |

```toml
[server.auth]
jwt_issuer = "https://accounts.example.com"
jwt_audience = ["lore-service"]

[server.auth.jwk]
endpoint = "https://accounts.example.com/.well-known/jwks.json"
```

## Store settings

Lore Server keeps three stores: an immutable store for content-addressed fragments, a mutable store for branch pointers, and a lock store for distributed locking. Each is configured by a top-level table — `[immutable_store]`, `[mutable_store]`, `[lock_store]` — whose `mode` field selects the backend.

| Table | `mode` default | Available modes |
| --- | --- | --- |
| `[immutable_store]` | `local` | `local`, `composite`, `replicated`, `remote`, or a plugin name such as `aws`. |
| `[mutable_store]` | `local` | `local`, `remote`, or a plugin name such as `aws`. |
| `[lock_store]` | `local` | `local`, or a plugin name such as `aws` (DynamoDB). |

Each mode reads its settings from a matching sub-table. The `local` mode uses `[immutable_store.local]`, `[mutable_store.local]`, and the in-memory local lock store. Plugin modes such as `aws` read from `[plugins.<name>]` (see [Plugin backends](#plugin-backends)).

### Local store mode

`[immutable_store.local]` and `[mutable_store.local]` configure the on-disk local stores. The mutable store accepts only `path` and `flush_delay_seconds`; the immutable store accepts the full set below.

| Field | Default | Applies to | Description |
| --- | --- | --- | --- |
| `path` | derived `<temp>/lore-server` | both | Filesystem location for the store. When empty, the server derives a path under the system temporary directory at startup (ephemeral — see [Zero-config defaults](#zero-config-defaults)). |
| `flush_delay_seconds` | `10` | both | Seconds between background flushes to disk. |
| `max_capacity` | none | immutable | Maximum number of entries before eviction. |
| `max_size` | none | immutable | Maximum total store size in bytes before background compaction reclaims space. |
| `eviction_delay` | none | immutable | Interval in milliseconds between eviction passes. Defaults to 10 s when unset. |
| `compaction_delay` | none | immutable | Interval in milliseconds between compaction passes. Defaults to 24 h when unset. |
| `target_capacity_percentage` | `70` | immutable | Capacity-utilization target (percent) that eviction reclaims down to. Applies only when `max_capacity` is set; 0 or ≥100 is treated as `70`. |
| `target_size_percentage` | `70` | immutable | Size-utilization target (percent) that compaction reclaims down to. Applies only when `max_size` is set; 0 or ≥100 is treated as `70`. |
| `compaction_parallel_groups` | `8` | immutable | Number of compaction groups processed in parallel. |

> [!NOTE]
> The `composite` mode layers a local cache tier over a durable plugin tier (for example, a local cache in front of S3). It reads `[immutable_store.composite.local]` and `[immutable_store.composite.durable]`, where the durable tier names a plugin mode such as `aws`. Composite and replicated store internals are out of scope here.

The lock store's `local` mode takes no sub-table — it uses an in-process, in-memory lock store (a single in-memory map) with no fields to configure. Because it lives entirely within one server process, it coordinates locks only on that node; distributed locking across a multi-node cluster requires a plugin backend such as `aws` (DynamoDB).

## Topology settings

`[topology]` selects how the server discovers its peers. The `provider` field is the selector.

| `provider` | Discovery model | Configuration |
| --- | --- | --- |
| `none` (default) | Single-node mode, no peers. | None. |
| `fixed` | Static, built-in peer list. | `[topology.fixed]`. |
| `rotating_id_fixed` | Static peer list whose IDs rotate on an interval (built-in). | `[topology.rotating_id_fixed]`. |
| `composite` | Merges peers from one or more topology sources (built-in). | `[topology.composite]`. |
| `consul` | Dynamic service discovery (plugin). | Configured via `[plugins.consul]`. See [Plugin backends](#plugin-backends). |

When `[topology]` is omitted entirely, the server runs in single-node mode, the same as `provider = "none"`.

### Fixed topology

`[topology.fixed]` holds a static `peers` array. Each peer is a table with `address`, `port`, and `locality` — all three are required.

```toml
[topology]
provider = "fixed"

[topology.fixed]
peers = [{ address = "192.168.1.10", port = 41340, locality = "SameRegion" }]
```

| Field | Description |
| --- | --- |
| `address` | Peer hostname or IP address. |
| `port` | Peer port. |
| `locality` | Peer locality relative to this server: either `SameRegion` or `OtherRegion`. |

### Rotating-ID fixed topology

`[topology.rotating_id_fixed]` is `fixed` with periodically rotating peer IDs. It takes the same `peers` array plus a required `rotation_interval_seconds`.

```toml
[topology]
provider = "rotating_id_fixed"

[topology.rotating_id_fixed]
rotation_interval_seconds = 300
peers = [{ address = "192.168.1.10", port = 41340, locality = "SameRegion" }]
```

### Composite topology

`[topology.composite]` combines peers from several topologies. Its `sources` array holds nested topology configs, each with its own `provider` and settings.

```toml
[topology]
provider = "composite"

[[topology.composite.sources]]
provider = "fixed"
fixed = { peers = [{ address = "192.168.1.10", port = 41340, locality = "SameRegion" }] }

[[topology.composite.sources]]
provider = "consul"
```

## Plugin backends

Backends beyond the built-in store modes and topology providers documented above are supplied by **plugins**, selected with the same `mode` / `provider` fields.

> [!IMPORTANT]
> Plugins are compiled into the server binary, not loaded at runtime. The stock open-source `loreserver` registers **no** plugins, so selecting a plugin backend — `mode = "aws"`, `provider = "consul"`, and so on — fails at startup with a `PluginNotFound` error listing the plugins actually available (none, by default). Using a plugin requires a server binary built with that plugin registered. The `aws` and `consul` backends referenced elsewhere on this page are reference implementations that ship as source in this repository (the `lore-aws` and `lore-hashicorp` crates) but are **not** part of `loreserver`.

### How plugin selection works

Five kinds of backend can be supplied by a plugin, each chosen by a different field:

| Plugin kind | Selector | Built-in alternatives |
| --- | --- | --- |
| Immutable store | `immutable_store.mode` | `local`, `composite`, `replicated`, `remote` |
| Mutable store | `mutable_store.mode` | `local`, `remote` |
| Lock store | `lock_store.mode` | `local` |
| Topology | `topology.provider` | `none`, `fixed`, `rotating_id_fixed`, `composite` |
| Notification | `notification.mode` | `local` |

When a selector names a plugin rather than a built-in, the server hands that plugin its `[plugins.<name>]` table as raw configuration and lets the plugin parse it. The field names and defaults of `[plugins.<name>]` are therefore owned by the plugin — and by the binary that compiles it in — not by `loreserver`. If no plugin of that name is registered, startup fails with `PluginNotFound`; a malformed plugin table fails with `PluginConfigError`; and a plugin that starts but cannot reach its backend fails with `PluginInitError`.

### Reference plugins

This repository ships two reference plugin implementations as source. Both require a derived server binary that registers them; neither is compiled into `loreserver`.

- **`lore-aws`** backs the immutable store with Amazon S3 and DynamoDB, and the mutable and lock stores with DynamoDB. AWS credentials come from the SDK's default credential chain (environment, profile, instance role), not from the config file.
- **`lore-hashicorp`** provides a Consul topology that discovers peers from a service catalog. It only *reads* the catalog — each server must be registered in Consul externally (by your orchestrator or a Consul agent) — returns only passing or healthy nodes, and re-polls on a fixed interval.

> [!NOTE]
> Per-field configuration for these plugins depends on the binary that registers them and is out of scope for this page. A dedicated Server plugins guide — covering the plugin registry, the factory traits, and how to build a server binary with plugins compiled in — is planned.

## Hook system config

Hooks run custom logic at points in the server lifecycle. Each hook is configured in its own `[hooks.<name>]` table. Every hook table has an `enabled` flag (default `false`) plus fields specific to that hook.

Hooks fire at these points:

- **`BranchPush`** — before a branch push is committed.
- **`BranchCreate`** — before a new branch is created.
- **`BranchDelete`** — before a branch is deleted.
- **`RepositoryCreate`** — before a new repository is created.
- **`Obliterate`** — before content is obliterated.

A hook can veto an operation by returning an error, log or audit it, or trigger an external notification. The example below sketches a compliance hook that denies pushes to protected branch patterns and posts to a webhook on failure. It is illustrative — `deny_patterns`, `allowed_users`, and `webhook_url` are fields a *custom* compliance hook would define; the stock `loreserver` ships no such hook (see note below):

```toml
# Illustrative only — requires a custom "compliance" hook compiled in.
[hooks.compliance]
enabled = true
deny_patterns = ["^release/.*$", "^protected/.*$"]
allowed_users = ["admin", "release-bot"]
webhook_url = "https://compliance.example.com/notify"
```

| Field | Description |
| --- | --- |
| `enabled` | Whether the hook is active. Defaults to `false`. |
| Hook-specific fields | Each hook defines its own fields — for example `deny_patterns`, `allowed_users`, and `webhook_url` for a compliance hook, or per-event webhook URLs for a notification hook. |

> [!NOTE]
> The base `loreserver` binary registers no hooks. A `[hooks.<name>]` table is matched by name against hooks compiled into the binary: a *disabled* table for an unknown hook is ignored, but an **enabled** table naming a hook that isn't compiled in aborts server startup. Custom hooks implement the `Hook` and `HookFactory` traits in Rust and are wired in by `build.rs` at compile time (the registry is populated at startup).

## Telemetry settings

`[telemetry.logger]` configures log output. `[telemetry.exporter]` configures the OpenTelemetry (OTLP) exporter. `[telemetry.metrics]` and `[telemetry.traces]` tune metric collection intervals and trace sampling.

| Field | Default | Description |
| --- | --- | --- |
| `telemetry.logger.format` | `json` (shipped default) | Log line format. One of `ansi`, `json`, or `text`. |
| `telemetry.logger.output` | `stdout` | Log destination. One of `stdout`, `stderr`, or a `{ file = "<path>" }` table. |
| `telemetry.logger.enable_otlp` | `false` | Forward logs over OTLP in addition to the local output. |
| `telemetry.exporter.endpoint` | none | OTLP collector endpoint, for example `http://collector:4317`. |
| `telemetry.exporter.queue_size` | none | Export queue size. |
| `telemetry.exporter.timeout` | none | Export timeout in milliseconds. |
| `telemetry.additional_labels` | none | Extra key-value labels attached to exported telemetry. |

> [!NOTE]
> `[telemetry.exporter]` is optional as a whole, but when the table is present all three fields (`endpoint`, `queue_size`, `timeout`) are required — they have no individual defaults. Logs, metrics, and traces are exported over OTLP only when `[telemetry.exporter]` is configured; `telemetry.logger.enable_otlp` forwards logs only in that case.

### Metrics and traces

`[telemetry.metrics]` and `[telemetry.traces]` ship in `default.toml` with the defaults below.

`[telemetry.metrics]`:

| Field | Default | Description |
| --- | --- | --- |
| `export_interval_millis` | `30000` | Interval in milliseconds between samples of the async (Tokio) runtime metrics. |
| `sample_interval_millis` | `10000` | Interval in milliseconds at which the OTLP reader exports metrics to the collector; also paces local-store memory-stats reporting. |

`[telemetry.traces]`:

| Field | Default | Description |
| --- | --- | --- |
| `sample_rate` | `0.05` | Fraction of traces sampled. |
| `sample_rate_low_tier` | `0.001` | Sampling fraction applied to low-tier (high-volume) traces. |
| `service_name` | none | Service name reported on exported traces. Falls back to `lore` when unset. |

> [!NOTE]
> Both sample rates must fall within `[0.0, 1.0]` inclusive, or the server refuses to start with a configuration error.

## Notification settings

`[notification]` selects the notification backend.

| Field | Default | Description |
| --- | --- | --- |
| `notification.mode` | `local` | Notification backend. `local` uses in-process broadcast channels; any other value names a notification plugin. |

## Examples

### Run with zero config

```bash
loreserver
```

Starts a single-node server from the built-in defaults: an ephemeral self-signed certificate, a store under `<temp>/lore-server`, and the presigned URL feature disabled. Everything is local — the certificate is regenerated on each start, and the store lives in the OS temp directory, which a reboot can clear. For trying Lore out, not for keeping data.

### Persistent single-node server with a local overlay

Create a config directory with a `local.toml` that pins the stores to a fixed path and references real certificates:

```toml
# ./config/local.toml
[server.quic.certificate]
cert_file = "./cert.pem"
pkey_file = "./key.pem"

[immutable_store.local]
path = "/var/lib/lore/store"
flush_delay_seconds = 10

[mutable_store.local]
path = "/var/lib/lore/store"
flush_delay_seconds = 10

[topology]
provider = "none"
```

```bash
loreserver --config ./config
```

The `local.toml` overlay overrides the defaults, so the stores persist at `/var/lib/lore/store` across restarts.

### Select a non-default environment

```bash
loreserver --config /etc/lore/config --env dev
```

Layers `/etc/lore/config/dev.toml` over the built-in defaults, then `/etc/lore/config/local.toml`. A missing `dev.toml` is skipped, so this is safe even when only `local.toml` exists. The same selection works through environment variables: `LORE_ENV=dev LORE_CONFIG_PATH=/etc/lore/config loreserver`.

## See also

- [Deploy a local Lore Server](../how-to/deploy-local-lore-server.md) — stand up a persistent single-host server step by step.
- [Lore CLI configuration reference](lore-cli-config.md) — the client-side `config.toml` and `cli.toml`, distinct from the server settings on this page.
- [Quickstart](../tutorials/quickstart.md) — clone, stage, commit, and push your first revision.
