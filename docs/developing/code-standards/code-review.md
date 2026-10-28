# Lore code review standards

How to review a change to Lore.

## Guidance

Review against these rules. Each finding cites the rule it breaks. Anything `cargo fmt` or `cargo clippy` enforces isn't a finding.

- For design trade-offs, follow [engineering-principles.md](engineering-principles.md).
- For error handling, follow [errors.md](errors.md).
- For logging, follow [logging.md](logging.md).
- For task spawning, follow [tasks.md](tasks.md).
- For async code, follow [async-futures.md](async-futures.md).
- For tests, follow [testing.md](testing.md).
- For comments and documentation, follow [comments.md](comments.md).
- For architectural decisions, follow the ADRs in [decisions/](../decisions/README.md).
- For docs pages, follow the [doc review checklist](../doc-standards/operational/review-checklist.md).

## How to review

Review adversarially, from the code, not from assumptions, memory, or the change's description. Flag issues; don't change code.

1. Read each changed file in full, not only the hunks. Follow references outside the diff, such as callers, callees, and types, where needed.
2. Verify every claim in comments, docs, tests, and the commit message against the code and measurements.
3. Check that the code matches its intent: the CR description and any ADR, proposal, spec, or doc it implements. Intent docs may be in the diff or already submitted, so search `docs/developing/decisions` and `docs/proposals`. Flag code that does less than, more than, or other than they describe.
4. Check the change against [Guidance](#guidance).
5. Check for [irreversible changes](#irreversible-changes) and [performance](#performance) issues. These matter most.
6. Flag a contradicted ADR with no new ADR, an unrecorded decision, and a superseded ADR whose `status` isn't updated.
7. Check that a user-visible change has a `## Nightly` entry in `docs/release-notes.md`.
8. In async code, check that a restructured loop, `select!`, or match keeps its error precedence, draining, and channel-close semantics.
9. Report each finding with its location and a concrete failure. Where one fix is obvious, name it.

## Irreversible changes

Escalate only these to a human reviewer. Once released, only a migration can undo them.

1. **Protocol.** Protobuf definitions (`*.proto`) and the QUIC wire format: the bytes a client and server exchange. Server-internal QUIC handling, such as sockets, endpoints, and connection routing, isn't protocol.
2. **Serialized and immutable data.** `StateData`, `NodeBlockData`, `Metadata`, and similar. Anything in the immutable store or in `.lore/` files.
3. **Thresholds and limits.** Chunking parameters (`FRAGMENT_SIZE_MINIMUM` in `lore-storage`; `FRAGMENT_SIZE_EXPECTED` and `FRAGMENT_SIZE_THRESHOLD` in `lore-base`), the compression algorithm or default mode, and fragment flags.
4. **Public C API.** The `extern "C"` surface, `lore-capi/lore.h`, the cbindgen config, the event contract, and FFI error codes.

In the escalation comment, show how the change becomes irreversible. List the exact chain of events, from merge to the point where a revert no longer undoes it. Name the code, the data or peer it affects, and the step where the data is written or the peer depends on it. For example: a client built from this change writes `StateData` with the new field to the immutable store, a client built before it reads that `StateData` and fails, and reverting the change doesn't rewrite the stored data. If you can't name each step, don't escalate.

Then say which is missing: how old and new readers and writers handle each other's format, how existing data or peers migrate, the client and server ship order, and the rollback after new-format data is written.

Don't escalate for any other reason. Security, authentication, authorization, destructive commands, `--dry-run` behavior, change size, a missing LEP, and blocking findings are not reasons to escalate. Report them as findings. A revert can undo them.

## Performance

Performance, including allocation size and count, is the top priority. Review async code against [async-futures.md](async-futures.md) and its links.

Focus on repeated code: per fragment, file, node, request, or connection. Establish which from the callers. Once per process or per API call isn't a finding.

Flag:

- Allocation per iteration where per request would do: `to_vec()`, vec `clone()`, `format!`, `collect()`, a collection built in a loop. Say whether to hoist, reuse, or borrow.
- A collection, channel, or accumulator with no limit, sized by remote input, file count, or file size. Name the input.
- A changed limit, budget, buffer or chunk size, flush point, channel capacity, batch size, or concurrency cap. State the new ceiling.
- A whole-object read that replaces streaming, or has unbounded size.
- One server call becoming several, or a call moved into a loop.
- Blocking IO, compression, or hashing in `async` without `lore_spawn_blocking!`.
- A lock held across `.await`.
- Independent IO or server calls run in sequence that could run concurrently.
- A value recomputed in a hot path, such as a hash, parse, or path join, that could be computed once.

## Simplicity, re-use, and modularity

Check that the change uses existing code and is built from reusable parts.

- Flag a new function or type that duplicates existing code; it may have another name. Search for the same parameter and return types, other callers of the functions it wraps, and doc comments with the same purpose. Read the module list of its crate.
- Flag repeated code, in the diff or between the diff and existing code, and name the existing helper or the helper to add.
- Flag new code that isn't in the lowest crate that can hold it: `lore-base` for paths, strings, retry, allocation, and error types; `lore-io` for file IO; then the domain crate.
- Flag functions that differ only in a parameter, error type, or constant.
- Flag code a simpler form would replace without loss: dead code, an abstraction with one user, an unset parameter or option, an unreachable branch.
- Flag a string or integer where a type, such as an enum or newtype, would make invalid values impossible.
- Flag a change at the wrong layer: a caller hiding a fault in its callee, a special case where the general code should change, or logic in a crate or module that doesn't own it.

Check that the change can absorb new requirements. Flag a monolithic API or structure where small parts would do: a function with several jobs, a struct with unrelated state, or a type callers can't use without parts they don't need. Flag an API that exposes internals, because callers come to depend on them.

## Risk-adjusted testing

Match test coverage to risk.

- **Key business logic needs high coverage, with every edge case tested.** This is code where a wrong answer corrupts data, loses work, or admits the wrong caller: content addressing, chunking, compression, fragment and pack formats, merges and conflicts, branch and revision state, the name table, locking, auth, protocol encoding, and error code allocation. Flag an untested edge case or error path.
- **Trivial code needs little or no testing.** This is accessors, mechanical conversions, and forwarding code. Flag tests that only check trivial code.

Flag tests that add no assurance: a getter returning what a setter set, `is_ok()` where the value matters, or an assertion on error text instead of error type.
