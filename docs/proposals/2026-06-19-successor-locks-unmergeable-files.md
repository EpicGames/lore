---
lep: 2026-06-19-successor-locks-unmergeable-files
title: Successor Locks for Unmergeable Files
authors:
  - mattias.jansson
status: Draft
created: 2026-06-19
updated: 2026-06-19
discussion: https://github.com/EpicGames/lore/pull/39
---

# Successor Locks for Unmergeable Files

## Summary

This proposal adds causal exclusive locks for unmergeable files in Lore's free branching model. Each unmergeable file gains a server-side per-file "head" pointer naming the latest revision that modified it within a *lock scope*. An exclusive lock can only be acquired on a branch whose last-modification revision for the file (looked up via Lore's existing per-file-id back-pointer index) equals that head — i.e. the branch already contains every prior committed edit in scope. Each unmergeable file becomes one linear edit chain per scope, embedded in the larger DAG; the lock acquisition is the gate that enforces successor-only progress within the scope. Scopes are first-class server-side entities with their own identity and lifecycle, independent of any specific branch; each branch carries a `scope_id` in its metadata, inherited from its parent at creation. A repository has a default scope that all branches join unless they explicitly opt into another scope; a repository that only ever uses the default scope behaves identically to a global per-file chain (the degenerate case). The check is constant-bounded — a single revision-id comparison against Lore's existing back-pointer lookup — with no path-keyed state and no materialized (branch × file) indices.

## Motivation

Lore supports free branching: any user creates a branch, edits, and requests a merge. For files that merge by content (text, structured data), divergent edits across branches resolve at merge time. For unmergeable files — binary assets, opaque tool-managed formats — there is no such resolution: two divergent edits cannot be combined, so the only safe outcome is that one branch's edit becomes the new state and the other's work is discarded.

Lore's current exclusive-lock primitive prevents (through opt-in compliance) two concurrent edits to the same file by serializing access while the lock is held. It does not prevent the same file from being edited in parallel across branches over time. Locks are ephemeral: released on commit or push, after which any branch can acquire a fresh lock on the same file. Whether the lock is scoped globally or per-branch makes no difference — the contention is causal, not temporal. A user on branch B can hold an exclusive lock on file F, commit an edit, release the lock, and after this point in time a user on branch C — which has never observed B's edit — can acquire a new lock on F, edit it independently, and commit. The two edits now sit as competing tips of an unmergeable file's history. When B and C eventually merge, the conflict has no resolution path that does not silently lose work, even though the locks were correctly acquired and released.

The gap is not in exclusion. The current primitive correctly answers "is it safe to edit this file right now?" The gap is in causality: nothing answers "is it safe to take a new lock on this file from this branch — have all prior edits been made visible here?" Without that answer, exclusive locks alone cannot keep an unmergeable file's history coherent under free branching.

A second gap follows from the first. Even with a causality primitive in place, a single global chain per file over-couples lines of work that should not block each other. Bug fixes to an unmergeable file on a long-lived release branch would force every editor on main to be downstream of the release-branch fix before they could lock the file — and vice versa. The same coupling makes free-form experimentation impossible: a designer trying out alternatives on a throwaway branch would freeze edits to the same assets across the rest of the repository until the experiment was merged or abandoned. The shape of the second gap is partitioning: the chain mechanism needs a way to scope its causality so that independent lines of work do not interfere with each other's editability.

This matters now because Lore targets workflows with large unmergeable working sets — game assets, design files, generated artifacts — where the cost of an unresolvable conflict at merge time is far higher than the cost of upfront serialization, and where parallel release lines and isolated experiments are standard practice. Without both a causality primitive and a scoping primitive, lock-based protection of unmergeable files in Lore is structurally incomplete at any scale.

## Goals / Non-Goals

### Goals

1. **Causal-safety check at lock acquisition.** Before granting an exclusive lock on file F on branch B, verify that B contains every prior committed edit to F.
2. **Single linear edit chain per unmergeable file.** Committed edits to an unmergeable file form one totally ordered chain across the repository, regardless of how the surrounding DAG branches.
3. **Constant-bounded lock check at scale.** Tens of millions of files, tens of thousands of branches, high churn — lock check is one KV read plus one Merkle traversal, with no (branch × file) materialization and no global broadcast on push.
4. **Content-only chain advance.** Modifying the *content* of an unmergeable file (its BLAKE3 hash on the tree leaf node) advances the chain and requires the lock. Metadata-only changes — mode, timestamps, extended attributes — and path-of-record changes (rename, move) are *not* chain-advancing; they remain outside the lock protocol and merge through Lore's normal tree-merge mechanisms.
5. **Identity by file_id, never by path.** All cross-branch state keys on the stable file_id (which survives moves) — path is input/output only.
6. **Actionable lock-denial errors.** A denied lock names the revision, branch, and scope of the current head, so the user knows what to merge or sync.
7. **Scope-partitioned chains.** Independent lines of work — release branches, experimentation sandboxes — maintain independent chains for the same file. Edits in one scope never block lock acquisition in another. Scopes are separate entities with their own lifecycle; each branch carries a `scope_id` as metadata, defaulting to the parent's scope at creation. A repository that only uses the default scope behaves identically to the global per-file chain model.
8. **Server-mediated enforcement.** Lock validation and lock release are integrated into the Lore server's push handler — not advisory client-side checks. The server is the only path that mutates head and lock state, so clients cannot bypass the protocol by skipping the lock-acquire flow. This elevates the existing exclusive-lock primitive (which Motivation describes as relying on "opt-in compliance") to a first-class protocol-state mechanism.
9. **Policy-controlled strictness.** The strictness with which each primitive (lock state, chain head) gates operations is a repository-policy choice (see Enforcement policy in Proposed Design). Under the default strict policy, the server rejects pushes lacking the held lock or whose parent does not match the current head. Under advisory policy on either primitive, the server emits a warning and audit-logs in the same situations but allows the operation. The server remains the authority either way — what changes is the response, not whether the server is in the loop.

### Non-Goals

- Define what makes a file unmergeable. Assumed to be a per-file attribute Lore already tracks or sets out-of-band, or defined by file type/extension from repository policy.
- Auto-merge prior edits on the requester's behalf when the lock check fails. The proposal requires the merge to have happened; tooling can suggest it.
- Change locking behaviour for mergeable files. They remain unaffected and remain outside the locking mechanism.
- Eliminate the operational concerns around lock leases, heartbeats, and zombie cleanup. Those are orthogonal lock-state mechanics.
- Define the wire-protocol details of new RPCs. Scoped to the downstream spec.
- Define transparent (auto-acquired) locks and coupling to file modification notifications. Integrating lock acquisition with file-edit notifications — so a lock is taken automatically when a user opens an unmergeable file for editing, and released on the next push — is a follow-on design. The model proposed here supports such an integration directly (locks are server-side primitives that can be triggered by any client signal; push already releases the lock as a first-class step), but the notification protocol, IDE / tool integration, and UX details live in a future LEP.

## Proposed Design

Lore tracks two separate primitives, not just locks.

┌────────────┬───────────────────────────────────────────────────────────────────────────┬──────────────────────────┐
│ Primitive  │                             What it surfaces                              │      Temporal role       │
├────────────┼───────────────────────────────────────────────────────────────────────────┼──────────────────────────┤
│ Lock       │ "F is being modified right now by branch B"                               │ Present-tense, in-flight │
├────────────┼───────────────────────────────────────────────────────────────────────────┼──────────────────────────┤
│ Causality  │ "F has been modified up through revision X; your branch's view is at < X" │ Past-tense, settled      │
└────────────┴───────────────────────────────────────────────────────────────────────────┴──────────────────────────┘

- Locks address synchronization — coordination of ongoing work. A lock surface answers "is anyone editing F right now?" — which is useful for coordination, UI displays, "ping the lock holder" flows, etc.
- Causality addresses versioning — coordination of historical state. The change-tracking chain answers "is my version current?" — which is useful for status displays, pre-flight checks, "do I need to sync/merge before I start?"

These primitives are realized by two coupled mechanisms: a **scope system** that partitions causality across independent lines of work, and a **change-tracking chain** that records the per-(scope, file) sequence of committed edits. Each is detailed below; the lock-acquisition check joins them.

The **scope system** partitions locking causality into independent regions. A scope is a first-class server-side entity with a stable id and its own lifecycle, independent of any branch. Every branch carries a `scope_id` in its metadata; at branch creation, the new branch inherits its parent's `scope_id` by default, or names a different scope explicitly. Two branches in the same scope share lock causality for unmergeable files; two branches in different scopes do not. A default scope is created automatically at repository init; the repository's initial branch is assigned to it, so every subsequently created branch also lands in the default scope by inheritance unless it opts elsewhere. A repository that never creates other scopes operates as a single global causality region, the degenerate case.

The **change-tracking chain** is the causality primitive, the conceptual linear sequence of committed edits to an unmergeable file within a scope. Each link is a revision that modified the file; the chain is linear by construction because the lock protocol prevents divergent links (a new link can only be added from a branch that already contains the current tip). The chain itself is *not* materialized as a separate data structure — its links are just regular revisions in the revision graph, indistinguishable from any other commit. What the server materializes is one record per live `(scope_id, file_id)` pair: the **head**, a pointer to the chain's current tip (a revision hash). A branch is "caught up" on F in its scope when the most recent F-modifying revision in its history equals the head — answered directly by Lore's existing per-file-id back-pointer index, which walks file-history blocks to return, for any revision, the latest revision in its history that modified a given file_id (`lore-revision/src/revision.rs::find_last_modified_revision`, supported by the file-history machinery in `lore-revision/src/file/history.rs`). The check is a single revision-id equality. The chain advances by acquiring an exclusive lock, committing the edit, and pushing: the push atomically writes the new head value (in Lore's mutable store) and releases the lock. The chain is the protocol-level abstraction; the head entry plus Lore's back-pointer index are everything required to enforce it.

The lock-acquisition check joins the two mechanisms: a lock is granted only if the requester is caught up on the (scope, file) chain for its own scope. Cross-scope interaction happens only at merge time, where a merge bringing unmergeable-file edits across scope boundaries becomes a fresh chain advance in the target scope (see Cross-scope merges below).

*Figure 1. Three scopes, each with its own independent `head(F)` for the same unmergeable file F. Branches inherit scope from their parent at creation; lock acquisition compares the branch's last-modification revision for F against `head(F)` in the branch's own scope only.*

```
┌─────────────────────────────┐  ┌─────────────────────────────┐  ┌─────────────────────────────┐
│  scope: main                │  │  scope: release/1.0         │  │  scope: experiment          │
│  ──────────────             │  │  ──────────────────         │  │  ──────────────             │
│   main ──► feat-A           │  │   release/1.0 ──► hot-fix   │  │   sandbox ──► expt-A        │
│        ╲                    │  │                             │  │                             │
│         ─► feat-B           │  │                             │  │                             │
│                             │  │                             │  │                             │
│   head(F) = R8              │  │   head(F) = R12             │  │   head(F) = R3              │
└─────────────────────────────┘  └─────────────────────────────┘  └─────────────────────────────┘

  lock F on feat-A   →  caught-up check vs head(F) = R8   in scope `main`
  lock F on hot-fix  →  caught-up check vs head(F) = R12  in scope `release/1.0`
  lock F on expt-A   →  caught-up check vs head(F) = R3   in scope `experiment`

  chains across scope boundaries are independent — an edit in any scope
  does not affect lockability of F in any other scope.
```

### Scopes (Goal 7)

A scope is a server-side entity identified by a stable `scope_id`, carrying a human-readable name and its own lifecycle (create, rename, delete) independent of any particular branch.

Branches are *members* of a scope via a `scope_id` field in branch metadata. A branch's scope is fixed at creation: a branch created from a parent inherits the parent's `scope_id` by default, or names a different `scope_id` explicitly. Stacked branches inherit transitively through their parent chain. This proposal treats scope assignment as immutable after branch creation; reassigning an existing branch into another scope is left to a follow-on (see Unresolved Questions).

Every repository has a **default scope** created automatically at repository initialization. New branches whose parent is in the default scope join it. A repository that never creates additional scopes operates entirely within the default — functionally identical to a global per-file chain.

Scopes are typically created to isolate long-lived release lines (each release line is a scope, taking backports without blocking main) or to wall off experimentation (a sandbox scope where unmergeable-file edits do not propagate constraints to production work). Scope creation is independent of branch creation: a user creates a scope, then creates one or more branches assigned to it.

A third use case is the **short-lived isolation scope**, complementary to the default-scope degenerate case. The user creates a fresh scope, assigns a single branch to it, does work that should not interact with any existing lock chains — no causality check against any other scope's heads — and merges to a target scope when done. At merge time, cross-scope merge semantics apply: the merge becomes a chain advance on the target scope, requiring a held lock on the target for each unmergeable file the merge touches, and resolving conflicts there. This is an explicit escape hatch for one-off work that has to happen without waiting for other in-flight locks or chains — urgent hotfixes, throwaway prototypes, parallel "what-if" iterations on the same asset — and accepts at-merge conflict resolution as the trade-off. Mechanically identical to other scopes; the difference is intent and lifecycle. So the design has two degenerate cases at opposite ends: a repository using only the default scope (one global chain, maximum coupling) and a repository spinning up a per-branch isolation scope (no shared chain at all, conflicts deferred to merge).

Scopes are stored in Lore's mutable store the same way branches are, using two paired `KeyType`s that mirror the existing `KeyType::BranchMetadata` (id → metadata) and `KeyType::BranchId` (name → id) pattern. One maps `scope_id → scope_metadata_hash` (the scope's existence and metadata, e.g. `KeyType::ScopeMetadata`); the other maps `scope_name → scope_id` (the human-readable name, also serving as the "is this scope active?" lookup, e.g. `KeyType::ScopeId`). The `scope_id` is the canonical identifier — what branches store in their metadata, what head and lock entries key on. Names are purely a human-facing pointer to that id; renaming or detaching the name does not affect any other state.

Scope deletion is therefore a soft operation: removing the `scope_name → scope_id` mapping archives the scope (it disappears from `lore scope list` and name lookups fail), but the `scope_id`, its metadata, its head entries, and any branch metadata that references it all persist. Branches assigned to an archived scope continue to function — their lock checks still resolve against head entries keyed by the still-valid `scope_id`. Reinstating the name mapping (`lore scope restore <scope-id> <name>` or similar) brings the scope back into normal discovery. True purge (clearing the metadata and head entries too) is the heavier operation and would still require no live head entries and no member branches, but archive is the everyday lifecycle action and is fully reversible.

### Per-file head pointer (Goal 1, Goal 2, Goal 7)

Unmergeable heads live in Lore's existing **mutable store** (`lore_storage::MutableStore`), under a new `KeyType` (e.g. `KeyType::UnmergeableHead`). The store's native shape is `Hash → Hash` keyed by `Partition`, which matches the head data exactly:

```
partition: repository
key_type:  UnmergeableHead
key:       Hash(scope_id, file_id)   // collision-resistant derivation, distributes evenly across keyspace
value:     head_revision             // revision hash of the chain's current tip
```

Both key and value are 32-byte hashes. No new database, no schema — the mutable store handles durability and atomic single-key updates, and listing via its existing implementation. Storing the null hash for a key removes it (the existing store contract), which is exactly what the protocol needs for delete (see Content-only chain-link semantics). The number of entries scales with **currently-live (scope, file) pairs**, not the all-time count: entries are removed when the file is deleted in their scope and re-created on the next edit. Tens of millions of files across ~100 scopes fit comfortably in the mutable store's normal operating range.

`head_branch`, `updated_at`, `head_content_hash`, and similar fields are intentionally absent — they are derivable from the revision (via existing revision metadata and Lore's back-pointer index) and would only duplicate authoritative state. UX surfaces resolve them lazily on the error/display path. Keeping the value to a single hash is what lets the mechanism ride on the mutable store rather than requiring a separate database.

### Lock acquisition (Goal 1, Goal 3)

For `lock F` on branch B given path P:

1. Determine `scope_id = scope_of(B)`. Branch → scope is a branch metadata lookup.
2. Walk the Merkle tree from B.tip along P to the leaf, determining `file_id`. (Path-resolution failure here returns "no such file in your branch" before any lock state changes.)
3. Once `file_id` is known, run in parallel (both depend only on `file_id` and `scope_id`, not on each other):
   - **Back-pointer read:** read the back-pointer for `file_id` in B.tip's history → `branch_last_mod_revision`, the most recent revision in B's linear history that modified F.
   - **Lock insert:** insert an entry into the lock state for `(scope_id, file_id)`, failing with contention denial if an entry already exists. This is the actual mutex; holding it pins the head value for the validation that follows — no concurrent writer can advance the head without first obtaining this same lock.
4. After the lock insert in step 3 succeeds, load `head_revision` from the mutable store: `load(repository, Hash(scope_id, file_id), KeyType::UnmergeableHead)` → `head_revision` (or `AddressNotFound` if no entry exists).
5. If `branch_last_mod_revision == head_revision`, B is at the chain head for this scope — return success. If no head entry exists, this is first-edit semantics (post-delete resurrection or file's first edit in scope) — also return success. Otherwise: release the lock entry (we never had a chain-advancing claim) and deny with `(head_revision, scope_id)` in the error payload; tooling formats branch/author/timestamp lazily from revision metadata.

The ordering is deliberate: claiming the lock *before* reading the head eliminates a race in which a concurrent lock-edit-commit-push cycle could complete between an early head check and a later lock insert, leaving the requester holding a lock against a stale head. With this ordering, the head value observed in step 4 is stable for the rest of the operation because step 3 has already excluded every concurrent writer. The back-pointer read and the lock insert share only `file_id` as input, so they overlap rather than chain — the lock insert is dispatched as soon as the Merkle leaf yields `file_id`.

Cost: scope lookup (O(1) cached) + Merkle path resolution + max(back-pointer read, lock insert) + one mutable-store load. The validation-fails path adds one lock-entry delete; on the busy-file contended path that delete is unavoidable work and matches the rate of contention.

The above describes the **strict-default behavior**. Under advisory chain-enforcement policy the step-5 chain-behind denial becomes a warning and the lock is granted anyway; the audit trail records both the lock acquisition and the policy-allowed stale check. See Enforcement policy below.

### Batch lock acquisition (Goal 1, Goal 3, Goal 6)

A batch lock-acquire takes a list of paths `[P_1, P_2, …, P_n]` on branch B and acquires their locks in a single round-trip, with a caller-selected **mode** that controls partial-failure behavior:

- **Atomic** (the default) — all-or-nothing. Either every path's lock is granted, or none is, and the response is a single success/failure. The natural primitive for operations that must hold every lock to make progress: folder renames, multi-file commits, cross-file refactors. Partial acquisition would leave such operations in an unrecoverable half-state.
- **Best-effort** — independent per-path outcomes. The server attempts every path, returns a per-path result (granted, contention denied, chain-behind denied, not-found-in-branch), and the caller decides what to do with partials. Useful for pre-flight checks ("which of these can I lock right now?"), opportunistic acquisition, or bulk operations where per-path failures are tolerable.

The steps are the same in both modes; mode only changes how partial failures are handled:

1. Determine `scope_id = scope_of(B)`. Computed once for the whole batch.
2. For each P_i in parallel, walk the Merkle tree from B.tip along P_i to determine `file_id_i`. Path-resolution failures are reported per-path; in atomic mode any failure aborts the batch before any lock-state changes, in best-effort mode the failed path is marked not-found-in-branch and the rest proceed.
3. Once each `file_id_i` is known, run in parallel — both *within* each path (back-pointer read and lock insert share only `file_id_i`) and *across* paths:
   - **Back-pointer reads:** for each i, read the back-pointer for `file_id_i` in B.tip's history → `branch_last_mod_revision_i`.
   - **Lock inserts:** insert lock entries for each `(file_id_i, scope_id)`. In atomic mode the insertions across the batch are all-or-nothing — the implementation may sort keys to bound contention patterns under concurrent batches and releases any partial successes on conflict; in best-effort mode each insert is independent and conflicts produce per-path contention denials without affecting siblings.
4. After the lock inserts succeed, batch-load head values: `load(repository, Hash(scope_id, file_id_i), KeyType::UnmergeableHead)` for every i for which a lock was successfully inserted in step 3. Reads parallelize at the storage layer; the held locks pin every corresponding head value for the duration.
5. Validate every i: `branch_last_mod_revision_i == head_revision_i` (or no head entry exists, i.e. first-edit semantics). In atomic mode any chain-behind aborts the batch and releases all locks; in best-effort mode chain-behind is per-path — release that one lock, mark the path denied, leave the others held.

The response shape mirrors the mode: atomic returns either "all granted" or a single denial naming the offending path(s) and reason; best-effort returns a per-path map of outcomes (granted / contention / chain-behind / not-found), each carrying the relevant detail (head revision, scope id, conflicting lock holder).

Cost is linear in batch size in both modes, with the constant work — scope lookup, round-trip, response framing — amortized across the batch. Two layers of parallelism overlap: within each path, the back-pointer read and the lock insert run concurrently (sharing only `file_id_i`); across paths, the per-path work pipelines together. Path resolutions parallelize within the Merkle tree; lock inserts and head loads parallelize at the storage layer. Net latency stays much closer to "one lock acquisition" than to "n sequential acquisitions" for batches within the server's parallelism budget.

Tools performing operations on multiple unmergeable files should pick the mode that matches their need: atomic for operations that must hold every lock to make progress (where partial acquisition turns a folder rename into an unrecoverable half-state), best-effort for pre-flight or opportunistic patterns where per-path information is the value being requested.

### Why the revision-id check is sound

Lore's per-file-id back-pointer index records, for every revision in the graph, the most recent earlier revision in that revision's linear history that modified the file (by file_id) — implemented as the file-history-block walk in `lore-revision/src/file/history.rs`, surfaced by `find_last_modified_revision` in `lore-revision/src/revision.rs`. The chain invariant — only the lock-holder can produce the next chain link, and the lock-holder must already contain the prior tip — guarantees that the chain of modifications to F in a scope is linear. Therefore "B's most recent F-modifying revision" is well-defined and is a specific revision-id; comparing it to the head's `head_revision` answers chain-containment exactly. No bloom filters, no reachability bitmaps, no per-(branch, file) sparse matrix.

An alternative framing reaches the same answer: each chain advance produces a new tree entry for F (because the protocol-defined notion of modification is exactly what changes the tracked tree state), so the head's `head_content_hash` and B's content hash for F are equal iff `branch_last_mod_revision == head_revision`. The revision-id check is the direct form; the content-hash check is an equivalent surface that is useful if Lore's back-pointer index is ever unavailable or if a caller needs to verify the equality without consulting the back-pointer.

### Chain advance at push

**Deferred chain advance.** The head moves only at session conclusion — the push that releases the lock (or deletes the file). A locked session on F is the period from lock acquisition to lock release on F; it may span multiple pushes. Intermediate pushes from the lock-holder commit revisions to the holder's branch as normal but do **not** advance head — the chain link for the session is held back until the push that unlocks. This is a deliberate protocol property: it gives the Administrative force-unlock semantic its clean revert behavior (head still points at the prior concluded session), and it generates the "ahead of head" state described below where another branch in the scope can have synced an intermediate revision and still legitimately get a chain-behind denial.

When B pushes a commit C that modifies a set of unmergeable files (identified by `file_id`, not path) `{F_1, F_2, …, F_m}`, with `scope_id = scope_of(B)`:

The push carries an `unlock_files` parameter naming which of the modified files to unlock after the chain advance — either an explicit list (subset of `{F_1, …, F_m}`), the sentinel `all` meaning "every file this push modifies," or an empty list to keep every lock held. `all` is the default and matches the typical edit-commit-push-done workflow; specifying a subset is the opt-in for sessions that hold a lock across multiple pushes against the same file (so the user can keep the chain pinned for the next commit without re-acquiring).

For each `F_i` in `{F_1, …, F_m}`, the server runs the following in parallel — each `F_i`'s per-file work is independent of every other `F_j`:

1. Verify B currently holds the lock on `(scope_id, F_i)` (fresh read).
2. **Head update (conditional on session conclusion).** If this push concludes the locked session for `F_i` — that is, `F_i` is named in `unlock_files` (including via the `all` sentinel) *or* the push deletes `F_i` — write the head: `store(repository, Hash(scope_id, F_i), C, KeyType::UnmergeableHead)`. For a delete, the value written is `Hash::default()` (the null hash), which removes the entry per the mutable store's contract — matching the "delete removes the head" semantics described below. If the push modifies `F_i` but keeps the lock and isn't deleting, the head is *not* updated by this push; the chain advance is deferred until the locked session is concluded by a later push that unlocks (or deletes). Intermediate revisions still sit in the file's normal revision history, but they are not chain tips.
3. **Lock release (conditional).** If `F_i` is named in `unlock_files`, remove the corresponding lock entry. Otherwise the lock remains held; the next commit-and-push from the same branch can advance the chain again without re-acquiring.

No CAS on the head is needed — the lock is the sole concurrency control on head writes. As an inexpensive belt-and-bracers check, the server may verify that C's back-pointer for `F_i` (the prior F-modifying revision in C's history) equals the current head value before overwriting it; mismatch indicates either a lock-correctness bug or a client lying about its parent, and should fail loudly.

**Interaction with cross-branch back-pointer checks during a locked session.** Because head advance is deferred until the session concludes, a different branch in the same scope that syncs an *intermediate* revision from the locked session has, in its history, an F-modifying revision more recent than the (unchanged) head. Its `branch_last_mod_revision` is a descendant of `head_revision`, not equal to it, so the standard lock check denies with "chain-behind" even though the requester is content-wise ahead of head. This is the correct protocol behavior: only the lock-holder may advance the chain, and the chain hasn't advanced yet. In practice the requester is already blocked by contention denial (the lock entry exists), so they retry after the session ends; by then head has advanced to the session's final commit, and back-pointer comparison resolves normally.

Cross-key atomicity (so the push either fully succeeds across all `F_i`, or has no effect on any of them) uses the same distributed-commit mechanism Lore already employs for branch advance. Per-file work within that umbrella parallelizes naturally — the verify, conditional head write, and conditional unlock for each `F_i` are independent of every other `F_j`'s work, so the per-file dimension fans out across the storage layer.

The above describes the **strict-default behavior**. Under advisory lock-enforcement policy, step 1's lock verification becomes a warning if the lock isn't held and the push proceeds anyway; under advisory chain-enforcement policy, the belt-and-suspenders parent-mismatch check becomes a warning instead of a hard fail. Both transitions are audit-logged. See Enforcement policy below.

### Optional: collapsing quiescent heads

When all branches in a scope share the same back-pointer answer for F — i.e., every branch in the scope has observed the same most-recent F-modifying revision — the chain is *quiescent*. The head entry adds no information at that point: any lock attempt would succeed with its back-pointer matching the head, write the same chain forward, and update the head in place. Collapsing the head back to "no entry" is safe and reclaims storage; the next commit anywhere in the scope re-establishes the head via the existing first-edit semantics.

Mechanism:

1. Detect consensus for `(scope_id, file_id)`: for every branch B in the scope, gather `back_pointer(B.tip, file_id)`. If all equal `head_revision`, the chain is quiescent.
2. `compare_and_swap(head_key, head_revision, Hash::default())` — collapse the entry. The CAS predicate guards the case where a push advanced the head between observation and write; on CAS failure, defer.

Safety follows from the lock table being the actual mutex on chain advance, not the head entry. A deleted head is mechanically indistinguishable from a never-existed head: first-edit semantics still serialize concurrent acquirers through the lock-table insert-if-not-exists, and the first push after collapse re-establishes the head. A lock holder racing with the collapse is also benign — the CAS can null the head while the lock is held, and the holder's eventual `store(head_key, new_revision)` simply overwrites null with the new revision (no CAS needed on the push side because the lock is the sole legitimate writer).

Trigger strategy is deferred to the design phase (see Unresolved Questions): server-side periodic sweep, event-driven hook on likely-consensus moments (post-merge, branch deletion, syncing push), or lazy detection at next lock attempt are all viable; each has different freshness/cost trade-offs.

This optimization is layered, not load-bearing: the proposal is correct without it. Steady-state benefit is that storage tracks live *divergent* (scope, file) pairs rather than every live (scope, file) pair — usually a small fraction, since most files spend most of their time in consensus.

### Lock state — required contract (Goal 1)

What this LEP mandates is the contract a lock store must satisfy to implement the protocol. Storage choice — the existing `lore_revision::lock::LockStore` trait suitably re-keyed, a new mutable-store-backed implementation, or any other store — is downstream of this LEP.

**Key shape.** Locks are identified by `(scope_id, file_id)`. The chain invariant depends on two branches in the same scope contending on *the same* key when they want to lock the same file. Branch is *not* part of the key — it appears in the value (below). This is the structural reason the existing `LockStore` trait, which keys on `(repository, branch, hash)` with branch in the key, cannot be reused without adaptation.

**Required operations.**

- **Atomic batch insert-if-not-exists** across a set of `(scope_id, file_id)` keys, for atomic-mode batch acquisition. Either every key succeeds or none does; on partial conflict the implementation releases its partial acquisitions and reports the conflicting keys.
- **Independent batch per-key insert-if-not-exists** across a set of keys, for best-effort-mode batch acquisition. Per-key outcomes, no cross-key dependency.
- **Conditional batch release** of a set of keys, authorized by the pushing branch (which must match the current holder).
- **Holder read** for a given key, returning the current holder's `branch_id` or absent. Used by push-time verification and by lock-denial messaging.

**Value contract.** The entry must associate the held key with the holder's `branch_id` (enough for the two checks above: push-time identity-match and lock-denial holder display). The chain protocol depends on nothing else in the value; any further fields (acquired-at timestamp, lease expiry, sidecar metadata) are implementation choice.

**Concurrency contract.** Insert and release operations must be atomic with respect to each other and to themselves; the protocol relies on the lock being a true mutex for the head it pins.

**Out of scope.** Lease semantics (heartbeats, expiry, zombie cleanup), on-disk representation, the query surface beyond what the operations above require, and any rich-predicate lookups are not part of the contract — they are operational and implementation concerns that do not change the chain protocol.

### Enforcement policy (Goal 9)

The two primitives this proposal introduces are informational at the protocol level: the **lock entry** carries the signal "this file *is* being modified by branch B right now," and the **chain head** carries the signal "this file *has* been modified up through revision R; your branch is at R'." How strictly the server gates operations on those signals is a separate concern — a repository policy, not a protocol property. The proposal specifies the signals and the strict-default behavior; the exact policy mechanism (per-repository setting, per-scope override, branch-protection-style rules, or something else) is deferred to a follow-on.

A minimal viable mechanism is a per-axis flag in repository metadata that the server reads on each request — mechanically straightforward and adequate for the strict-by-default proposition this LEP makes. Richer alternatives (per-scope override, branch-protection-style rules) are valid extensions but not protocol-blocking; the choice is safe to defer alongside the rest of the repository-administration tooling.

The protocol exposes two independent strictness axes:

- **Lock enforcement policy.** Under **lock-strict** (the default), the push handler rejects any push that modifies an unmergeable file without the pushing branch holding the corresponding lock; lock acquisition still serializes at the lock store regardless of policy. Under **lock-advisory**, the push handler emits a warning and audit-logs the push but allows it to proceed. The lock entry continues to surface "who is editing" for coordination; what advisory mode disables is the server's refusal to accept pushes from non-holders.
- **Chain enforcement policy.** Under **chain-strict** (the default), lock acquisition denies if the requester's `branch_last_mod_revision` is not equal to `head_revision` (chain-behind denial), and push validation rejects pushes whose parent for F does not match the current head. Under **chain-advisory**, both points emit warnings and audit-log but allow the operation. The chain head continues to surface "is your view current" for staleness display; what advisory mode disables is the server's refusal to advance a divergent chain.

The two axes combine independently: a repository can run lock-strict + chain-strict (the safety-maximizing default), lock-strict + chain-advisory (must respect in-flight sessions, but stale-acquire is allowed), lock-advisory + chain-strict (no enforcement against off-protocol pushes, but chain integrity is still gated), or both advisory (the primitives become pure signals; closest to Git LFS's posture today). Every operation that proceeded under advisory policy is audit-logged with the same fields the strict-denial path would have emitted, plus the policy that allowed it — so operators can see what would have been denied under stricter policy and how often the relaxed policy is being relied upon.

Advisory mode preserves the protocol's chain-tracking machinery as a source of truth: the head still advances, the lock entries still come and go, and the audit trail still records who did what. What changes is only the server's response to off-protocol intent — *deny* under strict, *warn and record* under advisory. Clients cannot bypass the protocol either way; they can only operate within the strictness the repository has set.

### Administrative force-unlock

The protocol exposes an admin-only operation that forcibly releases a held lock without going through the normal commit-and-push cycle. This is the operational escape hatch for stuck or abandoned sessions — a workstation that crashed mid-edit, an account that left the team, an automated process that took a lock and never released it.

The mechanism is straightforward because the head is only updated when the locked session is *concluded* by a push (Chain advance at push, step 2): force-releasing a held lock simply deletes the lock entry. The head pointer is not touched. Any intermediate revisions the holder pushed during the session — commits that modified the file but did not include it in `unlock_files` — remain in the holder's branch revision history as ordinary commits, but the chain head still points at the revision from the *previous* concluded session. From the chain's perspective the operation is equivalent to reverting the file to its prior head: any client that locks F next sees the chain content as of the last concluded session, not the abandoned in-flight content.

The force-released branch is now in an "ahead of head" state for that file: its back-pointer points at an orphaned intermediate revision that is no longer the chain tip. Subsequent lock attempts from that branch are denied with the chain-behind error pointing at the older head; the error message can flag a force-release origin so the user understands their in-flight work was abandoned. Recovery options:

- **Accept the revert.** Merge or sync the new (force-released) state of F into the branch and replay the intended edits under a fresh lock.
- **Restore the abandoned work.** An admin who wants to preserve the holder's in-flight commits can acquire a fresh lock from the holder's branch (since the branch already contains a more-recent F-modifying revision than head) and push a chain advance that establishes the abandoned tip as the new head — closing the session that was force-released open.

Force-unlock authorization shares the `--supersede` model used for scope-administration overrides (see Unresolved Questions for the authorization story). Every force-unlock event is recorded in the head-advance audit trail (see Observability under Non-Functional Considerations) with admin identity, target `(scope_id, file_id)`, prior holder `branch_id`, and the head revision unchanged by the operation — so the operation is traceable even though it bypasses the normal commit-and-push flow.

The same primitive answers two operational needs: it cleans up zombie locks (the standard use case), and it provides an explicit "abandon this session and revert to last-concluded state" mechanism for situations where the lock holder's work should not enter the chain at all.

### Identity by file_id and scope_id (Goal 5, Goal 7)

All cross-branch state — head entries, lock entries, caches — keys on `Hash(scope_id, file_id)`, never on path. Rename and move are themselves chain advances (they change the tree entry), so they don't disturb file identity. B1 may see F at `assets/old/foo.uasset` and B2 at `assets/new/foo.uasset`; if both are in the same scope, both derive the same key and contend against the same head value; if in different scopes, they derive different keys and sit on independent chains. Path resolution happens at the request boundary against the requester's branch view and is naturally cheap given Lore's Merkle tree. Scope lookup is a single per-branch cached value.

### Content-only chain-link semantics (Goal 4)

Only changes to an unmergeable file's *content* (the BLAKE3 hash on its leaf node in the Merkle tree) advance the chain for the scope the push happens in. Metadata-only edits (mode, timestamps, extended attributes) and path-of-record changes (rename, move) produce new tree entries but are *not* chain-advancing — they do not require a lock and do not pass through the chain protocol. Cross-branch path divergence is resolved at merge time via Lore's normal tree-merge, the same way it works for mergeable files: `file_id` is stable across renames, so a merge sees the same file at two paths and picks one, content-consistency already enforced by the chain on the orthogonal content axis. Copy creates a new `file_id` and a new chain (in the active scope); F's chain in any scope is untouched.

Delete is a chain-terminating operation in its scope that **removes the head entry** for `Hash(scope_id, file_id)` rather than leaving a terminal entry behind — mechanically, the push writes the null hash to that key, which the mutable store treats as a key removal. A subsequent edit in the same scope on any branch where F still exists (branched from before the delete) finds no head entry, gets the lock unconditionally (first-edit semantics), and re-establishes the head on commit. The lock-state mutex serializes the first-edit-after-delete window: only one branch in the scope can hold the lock at a time, so the first to commit fixes the new head, and any further concurrent resurrection attempts go through the normal successor-only check against that new head.

Head-entry removal happens at push time when the pushed commit deletes F — not when a branch tip "no longer has F" by other means, which is meaningless under free branching. A feature branch that pushes a delete of F removes the head in its scope even if other branches in the same scope still have F. The theoretical safety property: a delete-vs-edit conflict between two branches always has a no-loss resolution because the user can elect to keep the edit (the delete carries no content state to discard, only an intent to remove). Resolving in favor of the edit preserves all work; resolving in favor of the delete is a deliberate user choice to discard work. Either way, the conflict has a defined outcome and no work is lost without explicit user direction. Deletions and resurrections in one scope are independent of any other scope's chain — F may be deleted on a release scope while remaining alive (and editable) on main's scope.

### Cross-scope merges

A merge that pulls commits from a branch in scope A into a branch in scope B is the one operation where scope boundaries are intentionally crossed. When the merge brings in commits that touch an unmergeable file F, the merge produces a new tree entry for F in scope B that is **not** derived from scope B's existing chain. The proposal treats this as a chain advance in scope B requiring a held lock on F in scope B (i.e. the `(scope_id, file_id)` key for the target scope) for the duration of the merge commit; the source-scope chain is not consulted. The cross-scope merge is a fresh chain link in the target scope, regardless of what F's history looked like in the source scope.

This preserves the per-scope chain invariant (every advance is a successor of the prior head in its own scope) and keeps cross-scope semantics straightforward: scopes never share chain state, only branch DAG ancestry. The cost is that backporting an unmergeable-file fix from main to a release scope requires holding the lock in the release scope (and the merger choosing to apply the change there), exactly as a fresh edit on the release scope would — which is the correct user-facing model.

### Lock-denial error surface (Goal 6)

The denial response carries `(head_revision, scope_id, suggested_action: "sync"|"merge", source_branch_hint)`. The CLI presents:

```
error: cannot acquire lock on assets/hero.uasset — branch is behind on this file in scope 'main'
  head revision: 9f3a...c12d (created on feature/lighting, 2026-05-14)
  scope:        main
  suggested:    lore sync   # if the head is reachable on this branch's upstream
                lore branch merge feature/lighting
```

When a lock fails because the branch is in a different scope than the requester expects (e.g., the user thought they were on a branch assigned to the release scope but it inherited the default scope from its parent), the error names the actual scope explicitly so the user can correct the branch choice or re-target.

`source_branch_hint` and scope display name are derived from revision and scope metadata, not from denormalized fields in the head entry.

## Compatibility

- **Wire format** — N/A. No changes to existing message encodings, framings, or content-address derivations.
- **Client/server protocols** — Additive. New server RPCs for causal lock acquisition (single and batch, with the atomic/best-effort mode parameter) and head queries; existing `lore lock acquire` gains a new lock-denial reason carrying head metadata. Push protocol gains a validation step for unmergeable-file head/lock state in the changeset (failure modes: lock-not-held, head-mismatch) and a new `unlock_files` parameter (subset list, `all` sentinel, or empty list) controlling which locks held by the pushing branch are released as part of the push. Default is `all`, matching the prior implicit "release on push" behavior.
- **On-disk format** — N/A for repository data. Unmergeable heads ride in Lore's existing mutable store under a new `KeyType` (e.g. `KeyType::UnmergeableHead`), following the store's native `Hash → Hash` shape. Scope metadata and scope-name lookup use two more `KeyType`s (`KeyType::ScopeMetadata`, `KeyType::ScopeId`) mirroring the existing branch storage pattern. Lock state lives in any store satisfying the lock contract specified in Proposed Design (the existing `LockStore` trait suitably re-keyed, a new mutable-store-backed implementation, or another conforming store — choice is downstream of this LEP). The repository's Merkle tree, fragment encoding, branch tips, and revision-record format are unchanged.
- **CLI and public API** — additions and behavior changes by command family:
  - **`lore lock`**
    - `lore lock acquire` gains a new failure mode `LORE_ERROR_CODE_LOCK_CHAIN_BEHIND` when the branch lacks head for the file in its scope, with error payload identifying the head revision and `scope_id`.
    - `lore lock release` is unchanged.
  - **`lore scope` (new subcommand family)**
    - `lore scope create <name>` — creates a scope, returns the new `scope_id`.
    - `lore scope list` — lists active scopes; `--all` includes archived.
    - `lore scope info <scope-id|name>` — displays scope metadata.
    - `lore scope rename <scope-id> <new-name>` — changes the display name.
    - `lore scope delete <scope-id>` — soft delete; drops the `scope_name → scope_id` mapping. `scope_id`, metadata, head entries, and branch references persist.
    - `lore scope restore <scope-id> <name>` — reinstates a name mapping for an archived scope.
    - `lore scope purge <scope-id>` — hard delete; clears metadata and heads too. Refused while any live head entries or branch references remain.
  - **`lore branch`**
    - `lore branch create` gains a `--scope <scope-id>` flag to assign the new branch into an existing scope; without the flag the new branch inherits its parent's scope.
    - `lore branch info` output includes the branch's `scope_id` and the scope's display name.
    - `lore branch delete` gains a precondition: refused when the branch is the current head-holder for any unmergeable file still alive elsewhere in its scope; requires `--supersede` or a forward-merge.
  - **Existing scripts** that only acquire and release locks in a single-scope repository continue to work; scripts that ignore lock-acquire errors will now hit denials they previously would not have.

## Non-Functional Considerations

- **Concurrency** — The exclusive lock is the sole mutex on head writes; no CAS is required on the head store. Lock acquisition uses the lock store's atomic insert-if-not-exists primitive. Concurrent lock attempts on the same `(scope_id, file_id)` serialize at the lock store. Concurrent push validations across files parallelize naturally — each head key is independent. Multi-file commits inherit Lore's existing cross-key atomicity mechanism for branch advance.
- **Memory** — Per-entry head state is one `Hash` value (~32 bytes payload in the mutable store). Lock-entry size is implementation-dependent but small — bounded by holder `branch_id` plus any sidecar lease metadata. Worst-case head storage is `live unmergeable files × scopes`; with a typical ~10 scopes and 100M live files, well within the mutable store's normal operating range. Practical sizing is far lower because most files exist in only a subset of scopes, and with the quiescent-head collapse optimization steady-state sizing is live *divergent* (scope, file) pairs — usually a small fraction of all live (scope, file) pairs, since most files spend most of their time in consensus. Lock check is constant memory per request; Merkle traversal (which also yields the back-pointer) is O(path depth). No structures scale with `branches × files`.
- **Statelessness** — Head entries live in Lore's existing mutable store under a new `KeyType` (`KeyType::UnmergeableHead`) and are durable. Lock state lives wherever the implementing lock store places it; the lock contract requires durability sufficient to honor lease semantics across process restart, but does not constrain the storage layer beyond that. Clients hold no new state.
- **Determinism** — Head advance is a deterministic function of `(lock holder, push contents)`. Same sequence of acquisitions and pushes yields the same head sequence. The lock check is a pure function of `(stored head value, branch's back-pointer answer for the file_id)`.
- **Observability** — The proposal introduces new server-side state (head entries, lock entries, scope entries) and new failure modes (causality denial, wrong-scope denial, lock contention, lease eviction) that operators need to monitor and debug. The design phase specifies a metrics surface covering at minimum: lock-acquisition success rate and denial rate broken down by reason (chain-behind, contention, not-found-in-branch, wrong-scope); head-advance rate per scope; lease-eviction events; quiescent-head collapse events (if the optimization is enabled); scope lifecycle events (create, rename, delete/archive, restore, purge); and mutable-store operation latencies for the new `KeyType`s. An append-only audit trail for head advances — recording `(revision, scope_id, file_id, branch_id, holder_identity, timestamp)` for each advance — is the operational counterpart to the durability of the head entries themselves; without it, "who advanced this chain" turns into archeology against revision metadata.

## Migration Plan

Proposed plan, but details to be ironed out in the implementation spec and plan.

**Phase 1 — Storage deploy.** Register the new mutable-store `KeyType`s: `KeyType::UnmergeableHead` for head entries, plus `KeyType::ScopeMetadata` (scope_id → metadata) and `KeyType::ScopeId` (scope_name → scope_id) following the same pattern as branches' `KeyType::BranchMetadata` / `KeyType::BranchId`. Provision the lock store per its chosen implementation — the existing `lore_revision::lock::LockStore` trait re-keyed to `(scope_id, file_id)`, a new mutable-store-backed implementation, or another store satisfying the lock contract. Create a default scope per repository (write metadata under its new `scope_id`, write `default` → `scope_id` as the name mapping), and stamp every existing branch with that `scope_id` in its metadata (single bulk write — branch scope membership is set once per branch). Existing locks remain on the current code path; new storage is dormant.

**Phase 2 — Head backfill.** For each unmergeable file currently alive on at least one branch tip, scan revision history once per scope to seed the head entry with the latest commit in that scope that modified it. Pre-enforcement repositories have only the default scope, so this collapses to one scan per file. Files deleted in all current branch tips within a scope get no head entry in that scope, matching the steady-state invariant that entries track only live (scope, file) pairs. Idempotent; can run concurrently with normal traffic.

**Phase 3 — Enforcement enable.** Lock acquisition consults the mutable-store head value and denies when behind. Pre-existing locks that were acquired before enforcement remain valid until released. Push validation activates head advancement. `lore scope create` and `lore branch create --scope <id>` become available for declaring further scopes and assigning branches to them; pre-existing repositories continue operating in the default scope until and unless they opt in.

**Rollback.** Disable enforcement (Phase 3 → Phase 2): clients again get pre-existing lock semantics; head entries continue to be written but not consulted on acquisition. Scope declarations remain stamped on branches but have no enforcement effect. Observable signal: a sustained rate of lock-denials with `LORE_ERROR_CODE_LOCK_CHAIN_BEHIND` that does not correspond to legitimate stale branches indicates a backfill or chain-state error.

## Security Considerations

The new mechanism does not change Lore's trust boundary. Head writes are server-side, gated by server-verified lock ownership; clients never directly mutate head entries. Lock acquisition flows through existing authentication and branch ACLs — a caller cannot lock a file on a branch they could not commit to today.

A malicious caller cannot construct a head that bypasses content integrity: head entries reference revisions that themselves go through the existing content-addressed validation. A malicious peer cannot poison head state for another branch because they cannot acquire the lock without satisfying the causal check. The worst attack a permitted-but-malicious user can do is hold a lock and refuse to release — which is the same denial-of-service the existing lock primitive already permits, handled by the same lease-and-force-release mechanism.

## Privacy Considerations

Head entries hold `(Hash(scope_id, file_id), head_revision)` — no user identity, no path. Lock entries (per the lock contract in Proposed Design) associate `(scope_id, file_id)` with the holder's `branch_id`; no user identity directly, and the human holder is derivable via the branch's existing metadata, which is already visible through existing lock-query mechanisms. No new user-identifiable data is collected, persisted, or made visible beyond what existing lock state already exposes. Deletion and redaction follow the existing revision and lock policies.

## Risks and Assumptions

**Assumptions**

- **Assumption:** Lore's per-file-id back-pointer index (file-history blocks; `lore-revision/src/file/history.rs`; surfaced via `find_last_modified_revision` in `lore-revision/src/revision.rs`) resolves "most recent F-modifying revision in B's linear history" in roughly the same cost as a Merkle leaf traversal — and is updated atomically as part of every revision that modifies the file. *Invalidated if:* the back-pointer requires a separate index lookup with materially different cost, or if it lags revision creation in a way that exposes stale answers to the lock check.
- **Assumption:** Lore's per-file-id back-pointer can be queried specifically for *content-changing* revisions (revisions where the file's BLAKE3 content hash changed), distinct from tree-entry changes that only updated metadata or path-of-record. *Invalidated if:* the back-pointer cannot distinguish content updates from metadata- or rename-only changes — in that case the protocol must filter at lookup time (skipping back-pointer hits whose tree entries match the previous content hash) or treat the divergence as a fallback path.
- **Assumption:** `file_id` is stable across rename, move, and content edit, and is unique per file across the repository. *Invalidated if:* a rename or move produces a new `file_id`, or `file_id` is recycled after deletion, in which case identity has to be re-keyed.
- **Assumption:** Per-file, per-scope lock-acquisition rate is bounded by human or coordinated automation rates (≤ a handful per second per file per scope). *Invalidated if:* an uncoordinated automated workload attempts thousands of lock cycles per second on one (scope, file) pair, making the single-key write a hot spot in the mutable store.
- **Assumption:** Lore already has a distributed-commit mechanism that can advance multiple branch-tip-like rows atomically. *Invalidated if:* multi-file unmergeable pushes have to invent a new atomicity protocol.
- **Assumption:** The number of scopes per repository stays modest (single digits to low tens). *Invalidated if:* workflows demand hundreds or thousands of scopes per repo, in which case per-scope entry count and scope-lookup caching strategies need re-examination.

**Risks**

- **Risk:** Deletion of a Lore branch that holds head for many files (the head revision becomes unreachable from any surviving branch in the scope, while the file itself remains alive on other branches at older chain links) leaves locks unacquirable until resolved. *Mitigation:* refuse branch deletion when the branch holds head for any unmergeable file that is still alive elsewhere in its scope; require `--supersede` (admin or explicit) or a forward-merge to clear the heads first. (File-delete cases do not trigger this risk because the head entry is simply removed via writing the null hash.)
- **Risk:** A hard scope `purge` while live head entries or member branches still reference the scope_id would orphan chain state and dangle branch references. *Mitigation:* `lore scope purge` is refused under either precondition; ordinary `lore scope delete` is a soft operation (archive) that drops only the name → id mapping, so chain state and branch references stay intact and the action is reversible via `lore scope restore`. Hard purge is the heavier operation and is only invoked when the scope is truly empty of live state.
- **Risk:** Users misplace work in the wrong scope (branch off main when they meant to branch off a release scope, or vice versa) and discover it only when a lock denial points at an unexpected scope. *Mitigation:* `lore branch info` surfaces scope membership prominently; `lore branch create` confirms the inherited scope; lock-denial errors name the actual scope so the mismatch is legible.
- **Risk:** Long-held locks on hot unmergeable files become a productivity bottleneck within a scope. *Mitigation:* lease-with-heartbeat plus admin force-release path (orthogonal lock-state concern, addressed by existing operational tooling). Across scopes the risk does not amplify — independent scopes do not contend.
- **Risk:** Phase 2 backfill races with concurrent edits, producing stale or wrong head entries. *Mitigation:* backfill writes use the mutable store's `compare_and_swap` with an expected value of the null hash (entry absent); concurrent edits during backfill always win, producing a current head.
- **Risk:** Stale or abandoned branches in a scope keep the quiescent-head collapse optimization from ever firing — the consensus check fails because at least one (forgotten) branch lags behind. *Mitigation:* the optimization's branch-set should be filtered to "active" branches (modified within some window, or holding a tip that isn't reachable from another live branch); abandoned branches are an orthogonal cleanup concern.
- **Risk:** Repository policy drifts toward advisory enforcement habitually, defeating the protocol's safety guarantees through configuration rather than through code. *Mitigation:* operator-facing dashboards built on the audit trail surface advisory-allowed events alongside what would have been denied under strict policy, making the cost of the policy choice visible; the policy-setting mechanism (out of scope for this LEP) is expected to gate the relaxation behind explicit configuration rather than a default-on switch.

## Drawbacks

- Every live (unmergeable file, scope) pair gains always-on server-side state that did not previously exist, even ones rarely edited; storage and operational tooling have to cover them indefinitely.
- Adds a new lock failure mode (causality denial) distinct from contention denial; users need to learn the difference between "someone else holds the lock," "you're behind on this file in this scope," and "you're in the wrong scope."
- Scope is a new concept users have to model whenever they go beyond the default — scope creation, branch assignment, and cross-scope merges all become explicit decisions.
- Downstream tooling that wants to rely on strict chain semantics — e.g. CI checks that assume the chain head is the authoritative latest revision — must either depend on the per-repository strictness policy or treat the chain as best-effort, complicating any integration that prefers not to be policy-aware.

## Alternatives Considered

### Per-branch ephemeral locks (status quo)

Keep existing exclusive locks, scoped per branch or globally, without a causality check. Trust users to merge before locking.

*Rejected because:* this is exactly the model Motivation argues is structurally incomplete. The harm of a missed merge is not visible until merge time, and the cost at that point — discarded work on an unmergeable file — is precisely what the lock was supposed to prevent.

### Auto-merge on lock denial

When the lock check fails, the server merges the head revision into the requester's branch automatically before granting the lock.

*Rejected because:* merging a revision brings in not just the F edit but the source branch's entire causal closure up to that point, including changes unrelated to F. The user must decide whether and how to accept those changes; this proposal makes the decision explicit by surfacing the denial instead of silently doing a non-trivial merge on behalf of the lock requester.

### Single-trunk model for unmergeable files

Disallow editing unmergeable files outside a designated trunk (e.g., `main`). All edits must happen on trunk.

*Rejected because:* it forces every artist or pipeline that touches an unmergeable file onto a single branch, eliminating the value of Lore's free branching for the workflows that most need it (long-running feature branches, parallel content streams, experimental work).

### Single global chain per file (no scopes)

Keep one chain per file across the entire repository. Lock check is always against the global head; release branches, experimentation, and main all share one chain.

*Rejected because:* a single global chain over-couples independent lines of work — a backport on a release branch blocks every editor on main, an experimental edit on a sandbox branch freezes the same asset for everyone else. The proposed design retains this model as the degenerate case (a repository that only uses the default scope operates exactly this way) while letting users opt into partitioning when their workflow needs it.

### Scope tied to specific branches (scope-as-branch-attribute)

Tie scope identity to a designated "scope branch" — the branch's identity *is* the scope. Membership is derived from descent: any branch derived from a scope branch joins its scope. No separate scope entity exists.

*Rejected because:* scope lifecycle becomes coupled to one specific branch's lifecycle. Deleting the anchor branch needs ad-hoc machinery to preserve or transfer scope identity; renaming a scope branch either loses chain history (if scope id was the branch name) or requires a separate stable id alongside the branch — at which point a decoupled scope entity is already implicit. Treating scope as a first-class entity removes these awkward dependencies: branches come and go, scopes persist as long as their chain state and branch references do.

### Materialized (branch × file) up-to-date matrix

Precompute, for every (branch, unmergeable file) pair, whether the branch is at head. Lock check is a single keyed read.

*Rejected because:* at 10K branches × tens of millions of files, the matrix (even sparse) has 10¹¹-scale write rates and enormous invalidation fan-out on each push. The Merkle equality check delivers the same answer in O(path depth) without materialization, so the matrix earns nothing.

### Bloom filters or reachability bitmaps for descendancy

Precompute commit-reachability structures to answer "does B contain head_revision?" directly.

*Rejected because:* the equality check on tree entries is strictly simpler and equally correct given the chain invariant — Merkle entries already capture chain position. Reachability bitmaps remain useful for general history queries but are not required for this one.

### CAS on the head entry

Treat head as a contended write target, advance via the mutable store's `compare_and_swap` at push.

*Rejected because:* the exclusive lock already guarantees that only the holder can advance head while it is held. No concurrent writer exists. CAS adds protocol complexity (snapshot field on lock acquisition, expected-value tracking on head writes) to defend against a race that the lock already prevents. CAS is still useful during Phase 2 backfill, where it provides safe absence-conditional writes; it just isn't needed in the steady-state lock-and-push protocol.

## Prior Art

- **[Perforce](https://www.perforce.com/manuals/p4guide/Content/P4Guide/resolve.lock.exclusive.html).** Exclusive locking comes in two flavors: the `+l` filetype modifier (prevents others from opening for edit) and the `p4 lock` command (prevents others from submitting). Both are *per-branch / per-stream*: the same logical file in two different streams can be independently locked, edited, and submitted, and the divergence surfaces only at integration. Under Perforce's stream model (mainline, release, development, task, virtual stream types with parent-child hierarchy and copy-down / merge-up flow), this is structurally the same gap this LEP closes — Perforce shops that need cross-stream lock causality build custom server-side triggers that walk the stream hierarchy ([discussed at length on the perforce-user list](https://perforce-user.perforce.narkive.com/s3Mqig5m/p4-exclusive-checkout-across-branches)). Recent additions in Helix 2025.2 introduce a "global exclusive lock" for the DVCS workflow (taken on `p4 edit --remote=remote` and held until push or revert against the shared server), a partial answer scoped to personal-server topologies rather than general stream-graph causality. The lesson: as soon as branching enters the picture, lock primitives need branching-graph awareness to be sound; Perforce demonstrates this by negative example, lacking that awareness in the core protocol.
- **[Git LFS file locking](https://github.com/git-lfs/git-lfs/wiki/File-Locking).** Provides server-side locks for LFS-tracked files, held until released or pushed. Locks are **repository-wide and keyed by file path**, the opposite of Perforce's per-branch scoping: a lock taken in one branch prevents *other* users from editing that path on any branch ([explainer](https://www.vikram.codes/blog/2024/3/22/git-lfs-file-locking)). This avoids the divergent-edits-across-branches failure mode that Perforce streams have, but introduces three weaknesses this LEP avoids by construction:
  - **Path-keyed identity:** because the lock is keyed by file path, renaming a locked file loses the lock — "when you rename an exclusively-locked file, the lock is lost. You'll have to lock it again to keep it locked." This LEP keys on `file_id`, which is stable across renames.
  - **No causality at acquisition:** the cross-branch lock blocks *other users* on other branches, but does not check whether the *requesting branch* has observed the latest committed edit before granting. A stale branch can acquire and edit, and the divergence is detected at push or merge rather than at lock acquisition. This LEP makes the causality check primary.
  - **Cross-branch overreach with no scoping primitive:** because every lock is repository-wide, a release-branch hotfix on a binary asset blocks every other branch from editing that asset until it's released or merged — the [exact merge-friction problem](https://gitlab.com/gitlab-org/gitlab/-/issues/224462) Git LFS users report. GitLab's "two modes" (exclusive vs. default-branch) is a workaround; this LEP solves it with first-class scopes that branches join independent of the branch graph. The lesson: a *repository-wide path-keyed lock* over-blocks; a *per-branch lock* under-blocks; this LEP's *per-scope file_id-keyed chain* is the construction that does neither. Also worth flagging as a cautionary tale: [the GitHub web UI bypasses LFS locks entirely](https://dev.to/devactivity/unlocking-productivity-why-githubs-web-ui-must-respect-git-lfs-locks-2afb) — an enforcement gap that demonstrates the importance of routing every mutation path through the lock check rather than trusting clients to consult it.
- **[Plastic SCM / Unity Version Control smart locks](https://docs.unity.com/en-us/unity-version-control/smart-locks).** Server-side cross-branch awareness for locks, denying a lock when a newer version exists elsewhere. The closest known analog to this proposal; informed by the same game-asset workflows. Its **multiple destination branches** capability is the closest existing precedent for the scope concept here: each destination branch is an independent lock scope, locks on the same file may be held simultaneously across destinations without conflict, and check-ins on non-destination branches produce a `Retained` state requiring merge-to-destination — a different take on the cross-scope merge problem this LEP resolves with chain-advancing merges into the target scope. This proposal considers its design superior because it decouples causality scoping from branch hierarchies: the user acquiring a lock does not need to name or know about a destination branch — the scope is determined by the requesting branch's own scope membership (set once at branch creation), so lock-time UX needs only "lock this file," not "lock this file targeting that destination." The branch graph and the lock-causality partition are independent dimensions, where Plastic conflates them.

## Unresolved Questions

- **Multi-shard push atomicity mechanism.** 2PC, saga with compensation, or co-shard-by-commit forcing. Choice affects throughput and operational complexity.
- **Cherry-pick and revert of unmergeable-file edits.** A cherry-pick may produce a tree entry that doesn't match current head in the target scope. Denied at apply time, or allowed under a chain-advancing variant requiring a held lock? Likely the former; design phase decides.
- **`--supersede` authorization.** Who may roll a head back when a Lore branch holding head for a still-alive file is deleted: branch owner, repo admin, anyone with write access? Same question applies to scope deletion forcing-precondition overrides. Affects the day-to-day recoverability story.
- **Branch scope reassignment.** This proposal makes scope assignment immutable after branch creation. Open: should a branch be reassignable into a different scope under constraints (no active locks held by it, no head entries pointing at revisions reachable only via it in the old scope)? Reassignment would simplify some workflows (folding an in-flight feature into a sandbox scope) but adds consistency rules. The follow-on can decide the conditions and the mechanism.
- **Cross-scope merge ergonomics.** The proposal makes cross-scope merge a chain advance on the target scope. Open: does the merge surface require per-file confirmation when unmergeable files are involved, present a summary before proceeding, or silently chain-advance after a lock check? Affects how backports feel in practice.
- **Scope merge / split.** Can two scopes be merged into one (e.g., when a feature scope's work is folded back into main's scope)? Symmetric question: can a scope be split into two? Both are operations on chain state, not branches, so the design isn't obvious. Out of scope here; flagged for follow-on.
- **Default scope semantics.** Every repo gets a default scope at init. The dual-mapping storage model makes the answer almost mechanical — rename moves the `default` → `scope_id` name mapping; retire archives by removing that name mapping (existing branches in the default keep working via id); a wholesale replacement creates a new scope and archives the old. Open: which of these operations to expose as first-class CLI verbs, and whether the "default" name itself is reserved.
- **Quiescent-head collapse trigger.** The optimization's mechanism is defined; the trigger strategy is not. Periodic sweep (simple, but bounded staleness), event-driven (post-merge, branch deletion, syncing push — fires at moments most likely to produce consensus), or lazy detection (on next access, after the lock check observes a quiescent state) are all viable. Each has different cost / freshness trade-offs.
- **Enforcement policy mechanism.** The protocol defines two strictness axes (lock and chain) and a strict-by-default behavior, but does not specify how the policy is configured. Options include a per-repository setting in repository metadata, a per-scope override, branch-protection-style rules, or a global server-level default. Each shifts who controls relaxation (repo admins, scope owners, fleet operators); the LEP defers the choice to a follow-on. The protocol surface — what advisory mode does — is fixed here regardless of where the policy lives.
