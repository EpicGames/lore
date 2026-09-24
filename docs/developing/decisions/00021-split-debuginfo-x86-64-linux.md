---
status: accepted
date: 2026-08-08
deciders: Ryan Carbotte
---

# ADR-00021: Pack debug info separately for x86_64 Linux

## Context and Problem Statement

`.cargo/config.toml` set `rustflags` per target: `x86_64-pc-windows-msvc`, `aarch64-unknown-linux-gnu`, and
`x86_64-apple-darwin`/`aarch64-apple-darwin` each had their own `[target.*]` block. `x86_64-unknown-linux-gnu`
had none, so x86_64 Linux builds silently fell back to the generic `[build]` rustflags — a per-target table
fully replaces `[build]` rather than merging with it, so falling back meant losing every non-warning flag the
other targets carried: the `tokio_unstable`/`uuid_unstable` cfgs, the `force-unwind-tables`/`force-frame-pointers`
codegen flags, and, most significantly, `split-debuginfo`. With no `split-debuginfo` set, x86_64 Linux used
rustc's platform default of `off` — full DWARF **embedded directly in the binary**, not absent. The
`[profile.release-lto]` profile set `debug = 2` for every platform, so that embedded DWARF landed directly in
the `.so` instead of being split into a companion file, producing a release binary noticeably larger than
every other platform.

x86_64-unknown-linux-gnu needed a `split-debuginfo` policy. The two live precedents in the file disagreed:
`aarch64-unknown-linux-gnu` used `off` (embedding DWARF in the binary, splitting later via a downstream
packaging step), while `x86_64-pc-windows-msvc`, `x86_64-apple-darwin`, and `aarch64-apple-darwin` all used
`packed` (splitting at link time into a companion debug file).

## Decision Drivers

- Fix the file-size discrepancy the missing target block caused on x86_64 Linux.
- `split-debuginfo=off` on `aarch64-unknown-linux-gnu` is already known to exhaust the memory of the 16 GB
  arm64 GitHub-hosted runner when linking test binaries with embedded DWARF; that job is currently disabled
  in `pr-validate.yml` because of it.
- No downstream packaging step (e.g. the `package-cli.sh` referenced in the aarch64-linux comment) exists in
  this repo today to split embedded debug info back out for x86_64 Linux artifacts.
- Lore is distributed publicly on this target with no controlled deployment fleet to target, unlike
  aarch64-linux's curated Graviton3+ servers. The workspace's dominant SIMD-sensitive hot path — content
  hashing via `blake3` (v1.8.5) — already performs its own runtime CPU-feature dispatch (SSE2/SSE4.1/AVX2/
  AVX-512), independent of how the calling crate is compiled, so a `target-cpu` pin would narrow hardware
  compatibility without buying that path anything it doesn't already get for free.

## Considered Options

- `split-debuginfo=off` — match `aarch64-unknown-linux-gnu`, keep both Linux targets consistent with each
  other, embed debug info for later splitting by downstream tooling.
- `split-debuginfo=packed` — match `x86_64-pc-windows-msvc`, `x86_64-apple-darwin`, and `aarch64-apple-darwin`,
  split debug info into a companion file at link time via `objcopy`.

## Decision Outcome

Chosen option: `split-debuginfo=packed`.

It matches three of the four existing targets, gives a smaller default `.so` without depending on a
downstream splitting step that doesn't exist yet for this target, and avoids opting into the exact
linker-memory failure mode that already took down the `aarch64-unknown-linux-gnu` CI job. No `target-cpu` pin
is added. This target ships publicly with no controlled hardware fleet to bet on, and the hottest SIMD-sensitive
path in the workspace — BLAKE3 content hashing — already dispatches to SSE2/SSE4.1/AVX2/AVX-512 at runtime
regardless of how Lore itself is compiled, so a pin would trade away hardware compatibility for a speedup that
path doesn't need from Lore's own codegen. A pin remains available later if a controlled x86_64 deployment
fleet emerges, the same way `aarch64-unknown-linux-gnu` pins `neoverse-512tvb` for Graviton3+.

Re-enabling the disabled `linux-aarch64` CI job is out of scope for this decision — it shares the
`split-debuginfo` knob but is a distinct problem (test-binary linking memory, not release-binary size) that
deserves its own evaluation.

### Consequences

- Good, because x86_64 Linux release binaries are now materially smaller by default.
- Good, because x86_64 Linux is consistent with Windows and macOS instead of being the one target relying on
  an unset fallback.
- Neutral, because x86_64-unknown-linux-gnu and aarch64-unknown-linux-gnu — the two Linux targets — now use
  opposite `split-debuginfo` strategies; a future reader comparing the two blocks should look here rather than
  assume it's a bug.
- Bad, because if a downstream packaging step for x86_64 Linux debug info is built later (mirroring
  aarch64-linux's `package-cli.sh` intent), this target's DWARF is already split at link time rather than
  embedded, so that pipeline would need its own packed-debug-info handling instead of reusing the
  embed-then-split approach.

## Pros and Cons of the Options

### `split-debuginfo=off`

- Good, because it keeps both Linux targets on the same strategy.
- Good, because it preserves the option to split debug info downstream, the way the aarch64-linux comment
  describes for CLI binaries.
- Bad, because it produces the same oversized `.so` this decision exists to fix, unless a downstream splitting
  step is built for x86_64 Linux at the same time — and none exists yet.
- Bad, because it is the exact setting that already exhausted the 16 GB arm64 CI runner's memory for
  aarch64-unknown-linux-gnu; adopting it for x86_64-unknown-linux-gnu risks the same failure mode there.

### `split-debuginfo=packed`

- Good, because it matches Windows and both macOS targets, leaving aarch64-linux as the one deliberate
  exception rather than x86_64-linux being the accidental one.
- Good, because it produces a smaller `.so` immediately, with no dependency on tooling that doesn't exist yet.
- Good, because it avoids the linker-memory profile that already caused a CI outage on the sibling Linux target.
- Bad, because it diverges from aarch64-linux, so the two Linux targets are no longer directly comparable by
  binary layout.
