# Lore error handling standards

This document defines the standard patterns for error handling and error propagation across the Lore codebase.

## Overview

Lore uses a layered error handling approach:

1. **Domain-specific error enums** — Defined per module using `thiserror`.
2. **Extension traits** — For error transformation with automatic logging.
3. **Public error interface** — The `LoreError` enum exposed to API consumers (legacy; see section 4).
4. **EventError trait** — Translates internal errors to public errors (legacy; see section 4).

The canonical contract a C API consumer reads on a failure is the FFI error code on `status`, the return value, and the `error` detail on the `Complete` event. See [section 4](#4-ffi-error-reporting-contract). `LoreError` and `EventError` are legacy and kept only for transition.

---

## 1. Defining error types

All error types MUST use `thiserror`. Each module defines its own error enum:

```rust
#[derive(Debug, Error, PartialEq)]
pub enum ModuleError {
    #[error("Resource not found: {0}")]
    NotFound(String),
    #[error("Task failed")]
    TaskFailure,
}
```

**Guidelines:**

- Use `#[derive(Debug, Error)]` at minimum; add `PartialEq` for testability.
- Error messages should be user-readable (they appear in logs).
- Use tuple variants for dynamic context: `NotFound(String)`, `RemoteConnect(Context)`.

---

## 2. Public error interface (LoreError)

Defined in `lore-revision/src/interface.rs`. `LoreError` is the public error code returned across the FFI boundary; every internal error translates to one of its variants:

| Variant | Value | Meaning |
| --- | --- | --- |
| `InvalidArguments` | 1 | The arguments supplied to the operation were invalid. |
| `AddressNotFound` | 2 | A content-addressable object wasn't found in any store. |
| `FileNotFound` | 3 | A file path couldn't be resolved to a tracked node or found on disk. |
| `PayloadNotFound` | 4 | A payload blob wasn't found for the associated hash. |
| `SlowDown` | 5 | The backing store is overloaded; the caller should retry later. |
| `Oversized` | 26 | A blob exceeded a size limit enforced by the caller or the protocol. |
| `Internal` | -1 | All other errors. |

The `NotFound` (101), `AlreadyExists` (102), and `Connection` (103) variants are legacy categories kept for transition and will be removed.

---

## 3. EventError trait

Defined in `lore-revision/src/event.rs`. Domain errors in `lore-revision` that surface to users MUST implement this trait:

```rust
impl EventError for ModuleError {
    fn translated(&self) -> LoreError {
        match self {
            ModuleError::NotFound(_) => LoreError::NotFound,
            _ => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}
```

---

## 4. FFI error reporting contract

This section describes what a consumer of the C API reads on a failure. It is the canonical contract; the older `LoreError` and `EventError` paths above are legacy and kept only for transition.

### The code carries on `status` and the return value

A failed operation reports its FFI error code in two places:

- The synchronous entry points return the FFI error code as their `int32` result, and `0` on success.
- The `Complete` event carries the same code in its `status` field: `0` on success, the FFI error code on failure.

The synchronous return value and `Complete.status` always agree, because both derive from the same outcome. An asynchronous (`_async`) entry point returns `void`, so for those callers `Complete.status` is the only place the code arrives.

### The error detail on the `Complete` event

The `Complete` event also carries a `LoreErrorDetail` in its `error` field. It is the empty default on success and the populated detail on failure:

| Field | Type | Meaning |
| --- | --- | --- |
| `error_code` | `i32` | The error's FFI code. `0` on success, `-1` for an internal error. |
| `message` | `LoreString` | The error message. Empty on success. |
| `trace_locations` | `LoreArray<LoreTraceLocation>` | The captured trace, one entry per location. Empty when no trace was captured. |

Each `LoreTraceLocation` holds a `file`, a `line`, a `column`, and a per-location `context` string. A consumer reconstructs where the error was created or forwarded from these entries, without server logs.

`status` and `error.error_code` always hold the same value, by construction.

### `error_code` is canonical; `error_type` and `LoreError` are legacy

- `error_code` (on `LoreErrorDetail`, and the equal `Complete.status`) is the canonical code a consumer reads. It is the error's FFI code.
- `error_type` on the legacy `LoreErrorEventData`, and the `LoreError` enum, are legacy. They disagree with `error_code` for most errors. Do not use them for new consumers.

### No mid-stream `Error` event on a terminal failure

The library no longer emits a mid-stream `LORE_EVENT_ERROR` event on a terminal failure. The full error detail arrives on the `Complete` event instead. A failing operation delivers exactly one error-bearing event: the enriched `Complete`.

### Memory lifetime

The library owns all error-detail memory. The pointers a consumer reads from `LoreErrorDetail` and `LoreTraceLocation` (the strings and the trace array) are valid only for the single callback invocation that delivers the event. A consumer that keeps any of this data must copy it out before the callback returns.

---

## 5. Error extension traits

Defined in `lore-revision/src/error.rs`. These transform errors with automatic logging.

### Usage

```rust
use lore_revision::error::{LoreResultExt, LoreErrorExt};

// Transform error type with ERROR-level logging
store.get(key).emit_map_err(BranchError::StoreFailure)?;

// Transform with DEBUG-level logging (for expected failures)
store.get(key).debug_map_err(BranchError::NotExist)?;

// Return error directly with logging
return BranchError::InvalidName.emit();
return BranchError::NotExist.debug();
```

### When to use each

| Method | Log level | Use case |
| --- | --- | --- |
| `emit_map_err` | ERROR | Unexpected failures |
| `debug_map_err` | DEBUG | Expected failures (not found, already exists) |
| `emit()` | ERROR | Direct error return |
| `debug()` | DEBUG | Direct error return for expected cases |

The traits check the execution context's `failure` flag to prevent duplicate log cascades.

---

## 6. Panics and unwrap

**Never use `unwrap()`, `expect()`, or code that can panic in production code.** This is especially critical in `lore-server` where a panic can crash the entire server process.

```rust
// DON'T DO THIS
let value = map.get(key).unwrap();
let parsed: i32 = input.parse().expect("should be valid");

// DO THIS - propagate errors with logging
let value = map.get(key).ok_or(MyError::NotFound).debug()?;
let parsed: i32 = input.parse().emit_map_err(MyError::InvalidInput)?;
```

Acceptable uses of `unwrap()`:

- Tests (where panics are expected failure modes).
- Static initialization where failure is unrecoverable.

```rust
// Acceptable: regex is compile-time validated
static RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^\d+$").unwrap()  // Infallible: regex is valid
});
```

---

## 7. Crate-specific patterns

### Full pattern (thiserror + EventError + extension traits)

- **lore-revision**, **lore**

### thiserror only (no EventError)

- **lore-server** — Uses `tracing`; errors become gRPC/HTTP status codes.
- **lore-aws** — AWS-specific errors with a generic type parameter.
- **lore-base**, **lore-client**, **lore-credential**, **lore-error-set**, **lore-storage**, **lore-telemetry**, **lore-transport**.

### No custom error types

- **lore-proto**, **lore-macro**, **lore-notification**, **lore-hashicorp**, **lore-chaos-client**, **lore-capi**.

### anyhow usage

`anyhow` is allowed in **binaries only**, not libraries:

| Crate type | Error handling |
| --- | --- |
| Libraries (`lore-revision`, `lore-aws`, and others) | `thiserror` with typed errors |
| Binaries (`lore-server`, CLI tools) | `anyhow` allowed for convenience |

Libraries must expose typed errors so callers can match on specific error variants. Binaries are the end of the error chain and can use `anyhow` for simpler error aggregation.

---

## 8. Lore server errors

In lore-server gRPC handlers, use `warn_map_err`, `warn_error_to_status`, or `warn_mapped_error_status` when converting internal errors into a gRPC `Status`. All three log the original error at WARN level with additional structured fields, ensuring the cause and response are visible in our observability for investigation.

Prefer these helpers when the resulting gRPC status code is considered a server error as per the function `is_code_considered_server_error` (for example, an `Internal` status). These represent unexpected failures where observability over the original error matters.

Don't use them for expected, user-caused errors (for example, `NotFound`, `InvalidArgument`, `AlreadyExists`) where the status code alone is sufficient and WARN-level logging would be noise.

- **`warn_map_err`** — Use when you can chain directly with `?` on a `Result`.
- **`warn_error_to_status`** — Use when you already have the error value and need the `Status` before returning.
- **`warn_mapped_error_status`** — Use when you have already mapped the error to a `Status` (for example, inside a `map_err` closure where the mapping and logging steps must be done independently).

---

## 9. Best practices

1. **Never panic in production code** — Avoid `unwrap()`, `expect()`, and panic-inducing code.
2. **Always use thiserror** for error enum definitions in libraries.
3. **Use anyhow only in binaries** — Libraries must have typed errors.
4. **Implement EventError** for errors reaching the public API (`lore-revision` and `lore`).
5. **Use emit_map_err** for unexpected failures.
6. **Use debug_map_err** for expected/recoverable conditions.
7. **Use tracing** in server code, Lore macros in library code.
