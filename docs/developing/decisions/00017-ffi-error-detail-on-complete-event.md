---
status: accepted
date: 2026-06-18
deciders: Raghav Narula
---

# ADR-00017: FFI error detail on the Complete event

## Context and Problem Statement

The C API reported a failure in two limited ways. A failing operation sent a mid-stream `LORE_EVENT_ERROR` event. That event held a translated `LoreError` code and a message. The `Complete` event held only a flat `status`: `0` for success or `1` for failure. The synchronous functions returned the same flat `0` or `1`. For most errors the translated `LoreError` code did not match the error's real FFI code. A consumer could not tell where an error came from without reading server logs.

A consumer needs one reliable way to read a failure. One event should carry the real error code, the message, and the captured trace. The consumer should not need server logs.

## Decision Drivers

- A consumer should read the full error detail from one event.
- The code a consumer reads should be the real FFI code, not a translation that drops detail.
- The synchronous return value and the event status should never differ.
- A consumer should rebuild the file, line, column, and context of a failure without server logs.
- The layout change should only add fields, so old field reads keep working after a recompile.

## Considered Options

- Enrich the terminal `Complete` event and stop emitting `Error` on a terminal failure
- Enrich the mid-stream `Error` event
- Enrich both the `Error` and `Complete` events
- Add a formatted trace string instead of structured trace locations
- Widen the `EventError` trait to carry the code, message, and trace

## Decision Outcome

Chosen option: "Enrich the terminal `Complete` event and stop emitting `Error` on a terminal failure." It gives a consumer one error-bearing event with the full structured detail. The synchronous return value and `Complete.status` always match. The layout only gains new fields.

The decision has three parts:

- Put the full error detail on the terminal `Complete` event as a structured `LoreErrorDetail`: the code, the message, and a `LoreArray<LoreTraceLocation>`.
- Use the error's FFI code everywhere. On failure, `status`, the synchronous return value, and `error.error_code` all hold that code. On success they hold `0`.
- Stop emitting the mid-stream `LORE_EVENT_ERROR` event on a terminal failure.

### Consequences

- Good, because a failing operation sends one error-bearing event, the enriched `Complete`, and a consumer reads the code, message, and trace from it.
- Good, because the synchronous return value and `Complete.status` always match. Both come from the same outcome.
- Good, because the code is the real FFI code, not the `LoreError` translation that drops detail.
- Good, because the layout only gains fields, so a consumer that reads just the old fields still compiles after a recompile.
- Bad, because this changes the meaning of a documented contract. A consumer that branched on the old `{0, 1}` status must change. So must one that waited for an `Error` event on a terminal failure.
- Bad, because adding a field to the `Complete` payload grows the event union that holds it, so every consumer must recompile. The change is source-compatible, not binary-compatible.
- Neutral, because the `LoreError` enum, the `error_type` field, and the `LORE_EVENT_ERROR` variant stay defined. The library only stops sending `LORE_EVENT_ERROR` on a terminal failure.

## Pros and Cons of the Options

### Enrich the terminal `Complete` event and stop emitting `Error` on a terminal failure

- Good, because there is one error-bearing event, so it is clear which event carries the detail.
- Good, because `Complete` always fires. An async consumer cannot read a return value, but it still gets the code and detail.
- Good, because the change only adds fields and stays source-compatible.
- Bad, because consumers that read the mid-stream `Error` event must move to reading `Complete`.

### Enrich the mid-stream `Error` event

- Good, because the `Error` event already exists for failures.
- Bad, because `Error` does not fire on every path. A consumer would still need `Complete` for the status. That leaves two channels.
- Bad, because the synchronous return value and the `Error` event could still differ.

### Enrich both the `Error` and `Complete` events

- Good, because every existing consumer keeps working unchanged.
- Bad, because it carries the same detail twice, with two chances to differ.
- Bad, because it keeps two error channels instead of one.

### A formatted trace string instead of structured trace locations

- Good, because a single string is simple to carry.
- Bad, because a consumer cannot read the file, line, column, and context as fields. It would have to parse a string whose format is not a contract.

### Widen the `EventError` trait to carry the code, message, and trace

- Good, because it would reuse the existing error type.
- Bad, because many types implement the trait, so widening it changes every one of them.
- Bad, because reading the trace from the concrete error before it widens is cheaper. A small `HasTrace` trait covers the generic case and leaves `EventError` unchanged.

## More Information

The error code values are not yet stable to branch on. A table that maps each error code to a name ships in later work. Until then, treat `error_code` as opaque: test it for `0` versus non-zero, but do not hard-code specific values.

See the FFI error reporting contract in [docs/developing/code-standards/errors.md](../code-standards/errors.md).
