---
status: proposed
date: 2026-11-02
deciders: Mattias Jansson
---

# ADR-00028: State node operations behind one write boundary, with tiers by caller and a stress suite

## Context and Problem Statement

A `State` is a revision tree in memory: node records in blocks, each directory's children a chain threaded through their records. Every command that changes a tree changes a `State`, as do the C API's revision-tree verbs.

Writes to node blocks were spread across the code base: more than twenty writers outside `state.rs` changed records in place, and nothing stated what a writer had to preserve. The C API's verbs ran in parallel on one handle, staging walked directories in parallel, and a virtual file system (VFS) provider would serve many processes beside its own background work, any of them dropped at any await. Reviewing the writers found races and defects: moving a node could leave it in no chain, a lookup could miss a name moved behind its walk, a dropped operation could leave a dirty node below a clean directory for committing to miss, and the C API's delete verb freed slots parallel verbs still held. Nothing freed a detached slot, so saving stored each for good.

The question was how to keep the tree whole under concurrent writers and cancellation without slowing the paths every command runs.

## Decision Drivers

- Correctness: every invariant holds whatever runs beside a writer and wherever its future is dropped, or a caller owns the stated exception.
- Performance: no tree-wide lock, no allocation on hot paths, no runtime test of the caller, small futures, and no instruction-count regression beyond accepted budgets.
- A boundary the compiler enforces, as a writer that skips a step corrupts a tree without any error.
- Reproducible races, and tests that explore interleavings no author thought of.

## Considered Options

- Fix in place: keep the writers and fix each race at its site.
- One lock: serialize every writer of a tree.
- Node operations behind one write boundary: every change in one module, the only code that can write a node block, with per-block locks, lookups checked again under them, tiers by caller, and a stress suite.

Locks per directory, which need a lock table and an order across directories, and copy-on-write, which copies on every change, weren't weighed further.

## Decision Outcome

Chosen option: "Node operations behind one write boundary," because it alone meets both the correctness and the performance drivers. Fixing in place states no invariant, so the next writer reopens a race. One lock serializes the C API's verbs, staging's walks and a provider's requests, and an operation dropped half way still leaves a broken tree.

**The write boundary.** A node block's write methods are visible only inside `crate::state`, where the block type is defined, so a writer outside it doesn't compile. Beside the node operations in `lore-revision/src/state/node/`, only saving and loading, committing where it resets the root record, and clearing the flags above a discarded node write a block; tests write through `write_for_tests` and the write-path items a `test-util` build makes public.

**Tiers.** Callers fall into tiers by what runs beside them, and each tier keeps the guarantees its callers need and no more:

- **Served** (`State::served()`): requests on a State a VFS provider serves. Every chain change and flagging requires a parent not flagged `Discarded`, dirty and not about to be cleaned, so the dirty closure that settling directories and switching revisions rely on holds at every moment and after any cancellation.
- **Swap** (`State::swap()`): switching a served State to another revision in place, whose caller publishes only clean copies and detaches only clean nodes, empty directories and local files at their recorded place, so in place of the served rule the tier refuses only a copy that isn't clean and a parent flagged `Discarded`.
- **Unserved**: the methods on `State`, for the C API, staging and the other commands, which flag the parent and its ancestors dirty before detaching or reattaching a node. Under a shared claim no published slot is freed, so every node ID in flight stays valid.
- **Discard**: the C API's delete verb detaching a subtree its handle added, which flags nothing.

Saving a State is no tier: it runs no node operation, and no writer runs beside it.

No VFS provider in this repository serves a State; the served and swap tiers land because the contract is built around the strongest tier, and `state_bench` and the stress suite drive them as a provider would.

**Concurrency.** Each block has its own lock, and an operation write-locks only the few it changes, in ascending order, never across an await. It finds what it needs with no write lock held, then checks under its locks against a per-directory count of chain changes, where moving a node out weighs far more than adding or detaching one, or against a watched directory's log of changes. A failed check starts over a bounded number of times, then answers `SlowDown`. Permits order what a block lock can't: moving directories between parents, the C API's move and delete verbs, and the chain of blocks with free slots.

**Ordered steps.** Dropping an operation at any await leaves the tree consistent, but for the cases the contract leaves to callers (Limitations): a new node stays `Discarded` until it's published, detaching under a condition tests it before flagging, the C API's verbs flag the directories above a node before changing it, restoring a node excepted, which flags them right after, and flagging and clearing each write before they read.

**Detached slots.** A detached record keeps its `sibling` and its name, so a reader that copied its predecessor walks on through it and no node ID another task holds meets a reused slot. So instead of freeing a slot as it detaches, sweeping frees detached and leaked slots where nothing else runs on the State (contract clause C15, below):

- A block flags itself where a slot in it detaches or is taken, and when it loads, as an earlier session may have left one. Sweeping scans each flagged block in memory once and frees each detached root with its subtree.
- `State::node_sweep_detached` first frees what the C API's delete verb queued. Staging runs it at its end, committing through the CLI after discarding nodes, and committing through the C API after its last refusal, each before saving the State it works on; linked and layer States saved beside it aren't swept there. A slot a command leaves without sweeping is freed once sweeping finds its block in memory.
- Every chain mark and absence proof records the State's reclaim count, which sweeping moves where it frees a slot, and answers stale once it moved, so a provider needn't track its listings.

A list pushed wherever a node detaches would miss slots leaked in flight, and walking every block when saving would read the whole tree.

**The provider's obligation.** A served State never holds an exclusive claim, so a VFS provider sweeps it through `ServedNodes::node_sweep_detached`, only where every request, directory settle and revision switch it began has finished, none suspended part way, and it holds no copy it hasn't published, as sweeping frees every slot in flight. It supplies `retained`, its own test of whether the kernel still references a node, the kernel forgetting a node being the signal. A detached subtree with any node retained stays whole, to be swept later.

**The contract.** The documentation of `lore-revision/src/state/node/mod.rs` states the contract as numbered clauses, C1 to C15, each a guarantee the node operations keep whatever runs beside them; what it leaves to callers (Limitations); and the races and defects it closes, numbered H1 to H25 (H3 and H24 unused). The code, the tests and this record cite clauses and hazards by these numbers.

**The node record.** The record's reserved word, zero in every node written before, becomes `ext_flags` (`NodeExtFlags`), flags written and cleared with a node's dirty action. Size and layout stay, so no format version changes. Only tests, the stress suite and `state_bench` set the flags defined for a VFS provider, `LocalContent` and `Symlink`; an older build carries a set word through without clearing it.

**The stress suite.** The `state-hooks` feature adds a scheduling point between an operation's sections and logs every write section, and only the stress suite's builds enable it: the harness, the scenarios and its run of lore-revision's unit tests. Deterministic scenarios explore the orders of their steps, and `scripts/state-stress` runs mixes on many threads, each run judged by a model that replays the log.

### Consequences

- Good, because a writer outside the node operations doesn't compile, so every change goes through code that keeps the contract.
- Good, because the races and defects found are named, closed within the module but for H11's rename over a directory, and H19 and H25, which a VFS provider closes, and tested where reachable without a VFS provider. Among them are user-visible ones, such as `lore stage move` out of a directory keeping the file in both places.
- Good, because no lock covers the tree, so operations on different blocks run in parallel, and the tiers, generic parameters of shared bodies, cost nothing at runtime.
- Good, because staging and committing free the detached and leaked slots in memory before they save, and a session that only reads never sweeps.
- Good, because most `state_bench` workloads take 10% to 95% fewer instructions, and the C API's verbs in `revision_tree_bench` 0.25% to 64% fewer, than the State's writers before the module, as the VFS provider's branch held them, served operations included, with the benches run on them through adapters.
- Bad, because against the same writers the first served flagging below clean directories takes 1.42 times the instructions, and detaching below deep settled directories up to 1.08 times, within budgets of twice the count. The figures don't count sweeping, which adds to staging and committing a scan of each flagged block in memory.
- Bad, because the module is more code than the writers it replaces, every change to it must be checked against a long contract, and some futures awaiting it grew.
- Bad, because the served and swap tiers, the checked operations, the chain logs, `Absent` and sweeping a served State have no production caller until a VFS provider serves one.
- Bad, because a change count repeats after 65,536 nodes moved out of one directory, so a walk spanning that many can miss a name moved behind it; this is accepted.
- Bad, because a detached record keeps its slot and its name until sweeping frees it, on a served State only after the kernel forgot the node.
- Bad, because under heavy churn in one directory an operation can answer `SlowDown`, which no command retries and a C API host must: the C API reports it as `LORE_ERROR_CODE_SLOW_DOWN`.
- Bad, because the extension flags take the reserved word, so a flag with another lifetime needs a place of its own.
- Bad, because each node block in memory carries a change count per slot, 2 KiB per block, beside chain logs, pending-clear bits and a sweep flag.
- Bad, because the stress suite and the two benches add three examples, a scenario test target in each of two packages and five features that no CI job builds, so they're built and run by hand.

## More Information

[State node architecture](../internals/state-node-architecture.md) describes the module, and `scripts/state-stress/README.md` the stress suite.

A change to the module should be validated by the deterministic scenarios, a chaos campaign of the stress suite, and the instruction gate's counts of `state_bench` and `revision_tree_bench` against a baseline revision, which need a physical machine's hardware counter.

This decision should be revisited when a VFS provider serves a State, to confirm that the served and swap tiers hold what its background work relies on and that it sweeps as obliged, and if saving a State ever has to run beside writers, which the contract rules out today.
