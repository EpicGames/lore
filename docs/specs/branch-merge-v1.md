# Authoritative branch merge v1

`lore.revision.v1.RevisionService.BranchMerge` merges an immutable source tip
into an immutable target tip and publishes a normal two-parent Lore revision.
The server, rather than a web client, owns base selection, three-way diffing,
conflict validation, tree construction, and the final branch compare-and-swap.

## Lifecycle

1. Read both branch tips and retain their signatures.
2. Optionally call `ThinClientService.RevisionDiff` from the source signature to
   the target signature. Each three-way `DiffConflict` carries a stable
   `conflict_id`.
3. Call `BranchMerge` with both branch ids, both pinned signatures, and any
   target/source decisions.
4. If decisions are missing, the server returns `CONFLICTED` plus the missing
   ids and paths. It does not construct a revision or move either branch.
5. Submit all decisions. The server re-resolves the base and recomputes the
   merge, rejects unknown or duplicate ids, rechecks both tips, constructs the
   merge revision, then publishes through the protected BranchPush boundary.
6. A successful response is `MERGED`. An identical retry with the same UUIDv7
   returns the original revision even though the target now points at it.

If the source contributes no changes relative to the common ancestor, the
response is `ALREADY_UP_TO_DATE` and the target does not move. A stale source or
target signature is `FAILED_PRECONDITION`; disjoint histories are also rejected.

## Conflict semantics

A conflict pair is ordered `(source, target)` throughout the library and wire
API. `BRANCH_MERGE_SIDE_TARGET` keeps the target tree as-is.
`BRANCH_MERGE_SIDE_SOURCE` applies the complete source-side change, including
adds, deletes, moves, and directory changes.

Conflict ids are hashes over a versioned domain, the pinned base/source/target
signatures, and canonical source/target change data. They exclude node ids and
stream-local linked-repository indices. Consequently an id is reusable between
preview and merge only while the same tips and orientation remain pinned.

`BranchMergeResolution.resolution` is a `oneof`. A future field can carry an
uploaded/custom-content resolution while existing target/source clients remain
wire compatible.

## Atomicity and retry behavior

Immutable construction may leave unreachable CAS objects if a race is lost;
that is safe in Lore's content-addressed store. The only visibility point is
the existing protected BranchPush compare-and-swap. Branch protection, pre/post
push hooks, fragment verification, notifications, and revision-list
acceleration therefore remain on the same publication boundary as other
server-authored revisions.

Only publishable, fully resolved attempts retain an idempotency reservation.
`CONFLICTED`, `ALREADY_UP_TO_DATE`, stale-tip, validation, and construction
failures release it. This permits a client to reuse its request id when adding
decisions after a conflict response; once a revision is published, changing
the request under that id is rejected.

## Library API

The transport-independent pieces live in `lore_revision::merge_resolution`:

- `MergePlan::new(DiffResult)` attaches stable ids and selects executable
  source-side changes from Lore's joined presentation diff.
- `MergePlan::resolve` applies partial target/source choices and reports
  unresolved and unknown ids.
- `conflict_id` is shared by `RevisionDiff` and `BranchMerge`.
- `state::apply_tree_changes_for_commit` produces a settled target tree and
  deletion deltas.
- `commit::construct_merge_revision` freezes, rehashes, weaves history, writes
  metadata, and serializes a normal two-parent revision. An unchanged tree is
  valid because the second parent records the merged lineage.
