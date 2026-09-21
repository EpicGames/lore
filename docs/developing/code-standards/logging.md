# Lore logging standards

This document defines the standard patterns for logging across the Lore codebase.

## Overview

The codebase uses two distinct logging systems:

1. **Tracing crate** — For server and tool applications.
2. **Custom Lore macros** — For cross-platform library code.

---

## 1. Log levels

Defined in `lore-base/src/log/mod.rs`:

| Level | Value | Use case |
| --- | --- | --- |
| `None` | 0 | Disabled |
| `Trace` | 1 | Detailed diagnostics (compile-time gated) |
| `Debug` | 2 | Debug information |
| `Info` | 3 | General information (default) |
| `Warn` | 4 | Potential issues |
| `Error` | 5 | Error conditions |

---

## 2. Tracing (server/tools)

Used in: `lore-server`, `lore-chaos-client`, `lore-aws`.

### Usage

```rust
use tracing::{debug, info, warn, error, trace};

// Basic logging
info!("Processing request");
warn!("Operation failed: {e:?}");

// With structured fields (preferred)
debug!(rpc_status_code, elapsed_ms, "Lore success response");
error!(?error, "failed to send: {error:?}");

// With explicit target
trace!(target = "lore_server::store", "detailed info");
```

### Configuration

Control via the `RUST_LOG` environment variable. With `RUST_LOG` unset the server logs at `warn`
and above, so a server started without configuration still reports the conditions an operator has
to act on:

```bash
RUST_LOG=info                               # Above the default
RUST_LOG=error                              # Below it, for a large deployment
RUST_LOG=debug,lore_server::grpc=trace      # Module-specific
```

---

## 3. Lore macros (library)

Used in: `lore-revision`, `lore`, `lore-notification`.

### Usage

```rust
use lore_base::{lore_debug, lore_info, lore_warn, lore_error};

lore_info!("Processing branch: {}", branch_name);
lore_debug!("Store lookup for key: {:?}", key);
lore_warn!("Retrying operation, attempt {}", attempt);
lore_error!("Failed to connect: {}", error);
```

### Configuration

Defined in `lore/src/log.rs` via `LoreLogConfig`:

| Field | Default | Description |
| --- | --- | --- |
| `file` | 0 | Enable file logging |
| `file_rolling` | 0 | Enable daily rotation |
| `level` | Info | Minimum log level |
| `file_max_size` | 10MB | Max file size |
| `file_max_count` | 8 | Max rotated files |

**Default log paths:**

- macOS: `~/Library/Application Support/com.epicgames.lore/logs`
- Linux: `~/.local/share/lore/logs`
- Windows: `%LOCALAPPDATA%\Epic Games\lore\data\logs`

---

## 4. Crate-specific patterns

### Tracing crates

- **lore-server** — Full OpenTelemetry integration, correlation IDs in spans.
- **lore-chaos-client** — Pretty format to file, conditional console.
- **lore-aws** — Standard tracing macros.

### Lore macro crates

- **lore-base** — Defines the Lore logging macros that library code imports.
- **lore-revision** — Logging that routes through the event dispatcher.
- **lore** — `LoreLogConfig` and file rotation.
- **lore-notification** — Uses the macros from `lore-base`.

---

## 5. Best practices

1. **Use tracing** in server/tool code, **Lore macros** in library code.
2. **Prefer structured fields** over string interpolation in tracing.
3. **Use appropriate levels:**
   - `error!` / `lore_error!` — Failures requiring attention.
   - `warn!` — Potential issues, degraded operation.
   - `info!` — Significant events, request flow.
   - `debug!` — Diagnostic details.
   - `trace!` — Verbose diagnostics (use sparingly).
4. **Include context** — Correlation IDs, repository IDs, operation names.
5. **Avoid sensitive data** in log messages (tokens, credentials).

---

## 6. Key files

| File | Purpose |
| --- | --- |
| `lore-base/src/log/mod.rs` | `LoreLogLevel`, Lore macros |
| `lore/src/log.rs` | `LoreLogConfig`, file rotation |
| `lore-server/src/grpc/tower/tracing.rs` | Request span creation |
