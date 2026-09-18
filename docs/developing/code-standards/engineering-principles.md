# Engineering principles

These shape every change. They follow from where Lore runs: on machines you own, often left unattended, with nobody to restart a process that has stopped.

## 1. Engineer for performance

Avoid unnecessary buffer copies, serialization, and allocations on the hot path.

## 2. Engineer for correctness by construction

Choose data structures and process ordering that make correctness easy to reason about, rather than correctness you have to check for.

## 3. Engineer for simplicity

Avoid object-oriented and trait-heavy patterns in business logic. Prefer C-style functions plus POD data.

## 4. Engineer for determinism

Resource costs should be known and bounded for every process. A client must not be able to grow resource use without limit — memory above all.

## 5. Engineer for fault isolation

One key, file, store, or peer failing must fail locally and cleanly. Nothing a single client does may take down the server or another client's connection.

## 6. Engineer for observability

A problem is diagnosed from telemetry that was already emitted — you cannot go back and add a log line. Keep it free on the hot path: never format or allocate for a message that may be discarded (`lore_trace!` compiles away without the `trace_log` feature), and aggregate per-item detail into a single report. See [logging.md](logging.md).
