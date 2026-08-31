# Lore error handling standards

This document defines the standard patterns for error handling and error propagation across the Lore codebase.

## Overview

Lore uses a layered error handling approach:

1. **Discrete error types** — One `thiserror` struct per FFI code, all declared in `lore-base/src/error.rs`.
2. **Error sets** — Per-module `#[error_set]` enums naming the discrete types that module can produce.
3. **Forwarding** — Moving an error between sets with `.forward`, preserving variants and trace (see section 5).
4. **Public error interface** — The `LoreError` enum and the `EventError` trait (legacy; see section 4).

The canonical contract a C API consumer reads on a failure is the FFI error code on `status`, the return value, and the `error` detail on the `Complete` event. See [section 4](#4-ffi-error-reporting-contract). `LoreError` and `EventError` are legacy and kept only for transition.

---

## 1. Defining error types

There are two layers, and they are not interchangeable.

**Discrete types** live in `lore-base/src/error.rs`, one per FFI code. That file
is the single source of truth for code allocation; a new one takes the next free
number:

```rust
// FFI code 31
#[derive(Debug, Clone, Error, FfiError)]
#[error("invalid path: {path}")]
#[ffi_code(31)]
pub struct InvalidPath {
    pub path: String,
}
```

**Error sets** are per-module enums naming the discrete types that module can
produce. The variant name *is* the discrete type name, so the type must be in
scope:

```rust
#[error_set]
pub enum SetError {
    InvalidArguments,
    NotFound,
    InvalidPath,
}
```

The macro adds an `Internal` variant, `is_*()` predicates, `From` impls, and the
`forward` machinery.

**Guidelines:**

- Declare only the variants callers can actually act on. A set that names
  everything tells a caller nothing, and forces the same breadth on every set
  that forwards into it.
- Reuse a discrete type from `lore-base` before adding one.
- Error messages should be user-readable; they reach the caller as
  `LoreErrorDetail.message`.

---

## 2. Public error interface (LoreError)

Defined in `lore-revision/src/interface.rs`. `LoreError` is the public error code returned across the FFI boundary; every internal error translates to one of its variants:

| Variant | Value | Meaning |
| --- | --- | --- |
| `InvalidArguments` | 3 | The arguments supplied to the operation were invalid. |
| `SlowDown` | 31 | The backing store is overloaded; the caller should retry later. |
| `AddressNotFound` | 80 | A content-addressable object wasn't found in any store. |
| `PayloadNotFound` | 81 | A payload blob wasn't found for the associated hash. |
| `FileNotFound` | 82 | A file path couldn't be resolved to a tracked node or found on disk. |
| `Oversized` | 118 | A blob exceeded a size limit enforced by the caller or the protocol. |
| `Internal` | -1 | All other errors. |

Each value matches the `#[ffi_code(..)]` of the same-named struct in `lore-base/src/error.rs`, so the two agree for any code a caller reads. See [Error code allocation](#error-code-allocation) for how those codes are assigned.

The `NotFound` (101), `AlreadyExists` (102), and `Connection` (103) variants are legacy categories kept for transition and will be removed. They sit in the 100–109 range that `lore-base` reserves for them, so no discrete error type is allocated a code that collides with one of them.

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

### Error code allocation

Every discrete error type carries an `#[ffi_code(N)]`, and all of them are declared in `lore-base/src/error.rs`. The codes are allocated in blocks by the kind of failure they report, so a consumer can branch on a range when it only cares about the category and on the exact value when it cares about the specific error.

Every code is also a process exit status. The CLI returns the failing error's code from `main` as `ExitCode::from(code as u8)` (`lore-client/src/cli/client_main.rs`). That fixes two things: every code must fit in a `u8`, and no code may land on a value the shell or the OS already spends on something else. A `lore` command that exited 130 would be indistinguishable from one the user killed with Ctrl-C. The reserved rows below are those values, and no group block overlaps one.

| Range | Group | Meaning |
| --- | --- | --- |
| 0 | *reserved* | Success. |
| 1 | *reserved* | The CLI's own general failure (`ExitCode::FAILURE`), and the shell's generic error. |
| 2 | *reserved* | The CLI's usage exit, and the shell's "misuse of a builtin". |
| 3–15 | Input and validation | The request itself was malformed or names the wrong kind of thing. |
| 16–27 | Authentication and authorization | The caller isn't known, or is known and not permitted. |
| 28–39 | Connectivity and availability | The remote couldn't be reached or isn't currently serving. |
| 40–55 | Repository state | The repository is in a state that refuses the operation. |
| 56–63 | Already exists | Creating a resource that is already there. |
| 64–78 | *reserved* | The BSD `sysexits.h` codes (`EX_USAGE` through `EX_CONFIG`). |
| 79–99 | Not found | A named resource doesn't exist. |
| 100–109 | *reserved* | The legacy `LoreError` categories (101, 102, 103). |
| 110–117 | Configuration | Something the operation needs was never configured. |
| 118–125 | Resource limits | A size, depth, or efficiency bound was hit. |
| 126–128 | *reserved* | The shell's "found but not executable", "not found", and "bad argument to exit". |
| 129–192 | *reserved* | `128 + signal`, i.e. killed by a signal. 130 is Ctrl-C, 137 is `SIGKILL`, 143 is `SIGTERM`. |
| 193–254 | *reserved* | Free for future groups. |
| 255 | *reserved* | `Internal`, which is `-1` truncated to a `u8`. |

The trait carries a code as `i32` rather than a `u8` because `Internal` is `-1`, which isn't a code allocated here. A caller reading the library's `int32` return value or `Complete.status` sees `-1`; a caller reading the CLI's exit status sees 255.

**Adding an error type:** put it in the block that matches its group and take the next free code in that block. Never renumber an existing code to close a gap — the gaps are the headroom that keeps a group contiguous, and the tests hold every block clear of the reserved values, so anything taken from that headroom is a usable exit status. New groups come out of 193–254. A type whose name ends in `NotFound` goes in the not-found block even when it belongs to a subsystem with errors elsewhere: `PluginNotFound` sits with the other not-found codes, not with the other plugin codes. The tests at the bottom of `lore-base/src/error.rs` hold these invariants: register the new type in `registry()` there, and they check its code against its group's block, against every other code, and against the reserved ranges.

Codes can only be allocated in that file. `#[derive(FfiError)]` expands to a reference to `__ffi_code_registry_marker`, resolved where the derive is written, and only `lore-base/src/error.rs` declares it — so a `#[ffi_code(N)]` anywhere else fails to compile with `cannot find value __ffi_code_registry_marker in this scope`. An error type that never crosses the FFI boundary should not carry a code at all: use a plain `thiserror` enum rather than an `#[error_set]`, which would demand one for every variant type.

### Memory lifetime

The library owns all error-detail memory. The pointers a consumer reads from `LoreErrorDetail` and `LoreTraceLocation` (the strings and the trace array) are valid only for the single callback invocation that delivers the event. A consumer that keeps any of this data must copy it out before the callback returns.

---

## 5. Moving an error between types

Everything here comes from `lore_error_set::prelude`. Always import the prelude
glob rather than individual traits — see [section 5.4](#54-internal-is-a-compile-error-on-an-error-set).

`emit_map_err`, `debug_map_err`, `emit()` and `debug()` are **gone**. They logged
at the mapping site and carried no trace; the replacements carry a trace that
surfaces in `LoreErrorDetail.trace_locations` and log nothing.

### 5.1 Choosing

| Situation | Use |
| --- | --- |
| Source is an error set, target declares its variants | `.forward::<Target>("context")` |
| Source is an error set that over-declares | `.forward_any::<Target>("context")` |
| Source is foreign, caller can act on it | construct a named type from `lore-base/src/error.rs` |
| Source is foreign and unhandleable | `.internal("context")` |
| Translating one discrete type to another | `chain_err_from(source, "context")` |

The context string is mandatory and lands on the trace, so write what was being
attempted, not what failed.

### 5.2 `forward` and `forward_any`

`forward` is the default. It requires at compile time that the target declares
every variant of the source, so nothing is lost silently:

```rust
// StoreError declares every ProtocolError variant, so this is checked.
protocol::connect(url, identity, partition)
    .await
    .forward_with::<StoreError, _>(|| format!("connecting to remote store at {url}"))?;
```

The turbofish is usually required: `?` will `From`-convert from any type, so the
target parameter is otherwise ambiguous (`E0283`). It can be omitted in tail
position, where the function's return type pins it.

`forward_any` drops that compile-time check. Variants the target declares are
still preserved; only unmatched ones collapse to `Internal`. Reach for it when
the source declares variants the call site cannot actually produce, so
satisfying `forward` would mean widening the target with unreachable variants:

```rust
// StateErrors declares 44 variants; this call can produce a handful, and
// SetError should not grow branch and lock errors to accommodate it.
state::State::deserialize(repository.clone(), revision)
    .await
    .forward_any::<SetError>("deserializing state")?;
```

Prefer narrowing the source where that is affordable — then `forward` applies
again and the guarantee is back.

### 5.3 Don't discard the originating trace

When you deliberately translate to a different discrete type, chain rather than
construct from nothing:

```rust
branch::resolve(repository.clone(), branch.as_str())
    .await
    .map_err(|err| {
        ResetError::BranchNotFound(
            BranchNotFound { branch: branch.clone() }
                .chain_err_from(err, "resolving branch"),
        )
    })?;
```

To collapse an error set to `Internal` on purpose, say so explicitly with
`Target::internal_with_context(err, "context")`, which keeps the source.

### 5.4 `.internal()` is a compile error on an error set

`.internal()` is for **foreign** errors only (`io::Error`, `ParseIntError`, and
the like). Calling it on an error set would silently collapse every handleable
variant into `Internal` and report FFI `-1`, so `InternalForbiddenOnErrorSets`
makes it a compile error:

```text
error[E0034]: multiple applicable items in scope
   |
   = note: candidate #1 is defined in an impl of the trait `InternalForbiddenOnErrorSets` ...
   = note: candidate #2 is defined in an impl of the trait `WrapInternal` ...
```

Use `.forward` or `.forward_any` instead. Do **not** take the compiler's
suggestion to disambiguate with `WrapInternal::internal(..)` — that reinstates
the bug, and a pre-commit hook rejects it.

The guard only fires while both traits are in scope, which is why the prelude
glob matters. On a `Traced<_>` the message is clearer, and names `chain_err`.

#### A false positive to know about

If the receiver's error type is left to inference, it is still an inference
variable at method-resolution time, both traits apply, and you get the same
`E0034` on code that is actually correct. `try_into()` is the usual way in:

```rust
// E0034, even though the error is TryFromSliceError, a foreign type.
let magic = u32::from_le_bytes(buffer[0..4].try_into().internal("short blob")?);

// Fixed by naming the type so resolution can see it.
let magic = u32::from_le_bytes(<[u8; 4]>::try_from(&buffer[0..4]).internal("short blob")?);
```

---

## 6. Panics and unwrap

**Never use `unwrap()`, `expect()`, or code that can panic in production code.** This is especially critical in `lore-server` where a panic can crash the entire server process.

```rust
// DON'T DO THIS
let value = map.get(key).unwrap();
let parsed: i32 = input.parse().expect("should be valid");

// DO THIS - propagate a typed error
let value = map.get(key).ok_or(MyError::from(NotFound))?;
let parsed: i32 = input.parse().internal("parsing the retry limit")?;
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

### Error sets + EventError

- **lore-revision**, **lore** — errors reach the FFI boundary, so they also implement `EventError`.

### Error sets only

- **lore-server** — Uses `tracing`; errors become gRPC/HTTP status codes.
- **lore-aws** — AWS-specific errors with a generic type parameter.
- **lore-client**, **lore-credential**, **lore-storage**, **lore-telemetry**, **lore-transport**.

### lore-base

Holds the discrete `thiserror` types every set is built from. Defines no sets of its own.

### lore-error-set

The error framework itself. Provides `#[error_set]`, `Traced<E>`, `ChainError`, `Internal`, and `FfiError`. Not a consumer of `thiserror`.

### No custom error types

- **lore-proto**, **lore-macro**, **lore-notification**, **lore-hashicorp**, **lore-chaos-client**, **lore-capi**.

### anyhow usage

`anyhow` is allowed in **binaries only**, not libraries:

| Crate type | Error handling |
| --- | --- |
| Libraries (`lore-revision`, `lore-aws`, and others) | `#[error_set]` with typed errors |
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
2. **Use `#[error_set]`** for error types in libraries; `thiserror` remains only for the discrete types in `lore-base/src/error.rs`.
3. **Use anyhow only in binaries** — Libraries must have typed errors.
4. **Implement EventError** for errors reaching the public API (`lore-revision` and `lore`).
5. **Use `.forward`** to move an error between sets; it is checked at compile time.
6. **Use `.forward_any`** only when the source over-declares, and `.internal` only for foreign errors.
7. **Use tracing** in server code, Lore macros in library code.
8. **Don't discard originating error traces** — When constructing a new discrete error from a caught one, use `chain_err_from` (or `chain_err` if you've already destructured the variant) to carry the originating trace forward. Using `internal_with_context` is correct when collapsing to `Internal`; using `internal()` without context silently drops the source. Prefer `chain_err_from(source_err, "context")` over `Err(NewError { ... }.into())`.
