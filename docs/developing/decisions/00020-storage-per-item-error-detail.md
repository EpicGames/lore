---
status: accepted
date: 2026-09-16
deciders: Raghav Narula
---

# ADR-00020: Full error detail on the storage per-item events

## Context and Problem Statement

[ADR-00017](00017-ffi-error-detail-on-complete-event.md) put the real FFI code, message and trace on the terminal `Complete` event, and recorded that a translated code which drops detail is not what a consumer should read. It changed the call-level event only.

The storage API also reports every item of a batch separately, on its own `*_ITEM_COMPLETE` event. Those events carried a `LoreErrorCode`, a five-value enum. `storage_error_to_code` folded the thirteen `StorageError` variants into it, so `PayloadNotFound` was reported as `AddressNotFound` and eight other variants all became `Internal`. The event carried no message, so `Internal` was not diagnosable without server logs. The call level lost the same information again: each of the nineteen storage commands declared its own two-variant error set, and `build_call_error` reduced the item codes to `InvalidArguments` or `Internal`, replacing the message with a count.

## Decision Drivers

- A consumer should read the same detail per item that ADR-00017 gives it per call.
- A failing item should carry a message. `Internal` with no text is not diagnosable.
- A successful item should allocate nothing. Storage is a hot path.

## Considered Options

- **Carry a `LoreErrorDetail` on each per-item event, and return `StorageError` from every storage command.** Chosen.
- **Carry a plain `i32` FFI code.** Exposes the whole code space but still drops the message, so `Internal` stays undiagnosable, and it leaves two ways to report a failure.
- **Keep `LoreErrorCode` and add a detail beside it.** Carries the outcome twice, with two chances to disagree. The struct grows either way, so consumers recompile regardless and the compatibility it buys is small.
- **Widen `LoreErrorCode` with the missing variants.** Duplicates the `lore-base` code registry in a second place, and still carries no message.

## Decision Outcome

Chosen option: "Carry a `LoreErrorDetail` on each per-item event, and return `StorageError` from every storage command". It gives the per-item events the detail ADR-00017 defined for `Complete`, reuses that struct rather than inventing a second representation, and removes the mapping layers instead of correcting them.

- The ten per-item event structs carry `error: LoreErrorDetail` in place of `error_code`. They become `Clone` rather than `Copy`, because the detail owns its message and trace.
- Item handlers return `Result<(), StorageError>`. `storage_error_to_code`, `store_error_to_code` and `protocol_error_to_code` are deleted.
- Storage commands return `Result<_, StorageError>`. The nineteen per-command error sets and their `EventError` impls are deleted, so `Complete.status` is the failing item's own code.
- `build_call_error` selects the most actionable item failure by severity and forwards it, attaching the failure count as trace context.

`LoreErrorCode` stays defined and in use by the revision-tree events.

### Consequences

- Good, because a failing item reports its own code, message and trace, so it is diagnosable from the event alone. `PayloadNotFound`, `Disconnected`, `NotConnected`, `Maintenance`, `NotAuthorized`, `NotAuthenticated`, `NotFound`, `NotSupported` and `NoRemote` are now distinguishable, where the first was reported as `AddressNotFound` and the rest as `Internal`. `InvalidArguments`, `SlowDown` and `AddressNotFound` are unchanged: the old enum carried those three faithfully.
- Good, because a rejected argument now says which argument. It used to report a bare `3`.
- Good, because about 400 lines went away and nothing replaced them.
- Good, because a successful item allocates nothing. The default detail is a null pointer and a zero length.
- Bad, because it breaks the C ABI and the wire format. `LoreErrorDetail` is 40 bytes where the enum was 4, so a struct grows by 32 or 40 bytes depending on how its padding falls, and every consumer must recompile.
- Bad, because `copy`, `get_metadata` and `obliterate` still route a `ProtocolError` through `protocol_error_to_storage`, which folds authorization, maintenance and not-supported failures into `NotConnected`. Narrowing that has to move together with `lore-storage/src/read.rs`, which uses `NotConnected` as its stale-session retry trigger.
- Neutral, because the revision-tree events keep `LoreErrorCode`, so a second ABI break is queued behind this one.

## More Information

Dropping `EventError` from the storage path costs nothing, because it was already dead there: the trait is read only through `EventDispatcher::send_error`, which nothing in `lore/src/storage` calls. `storage_call` required it in its bound while building its detail from `FfiError`, `Display` and `HasTrace` alone. Removing the bound there and on the shared `no_repository_call` let the nineteen impls go, and relaxing a bound cannot break its other callers.

Ranking the error rather than its code means every variant that reaches the miss tier has to be named. A remote miss arrives as `NotFound` or `NoRemote`, where the old code-based ranking saw the `AddressNotFound` those had already been folded into. The first version of `severity` omitted them, which ranked a miss above `SlowDown` and let one absent key hide a throttled item in the same batch.
