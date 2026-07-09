---
lep: 2026-05-14-low-level-revision-api
title: Low-Level Memory-Based Revision Control API
authors:
  - mattias.jansson
status: Draft
created: 2026-05-14
updated: 2026-05-15
discussion: <TBD — fill in CR link when discussion CR is opened>
---

# Low-Level Memory-Based Revision Control API

## Summary

This proposal introduces a new Lore API surface — the low-level memory-based revision control API — bridging the gap between the existing storage system API (`lore::storage`, `lore_storage_*`) and the existing file-system-based revision control API (the file-system-driven `lore` CLI, `lore-capi`, and `lore` crate verbs). The new surface gives automated services and pipelines a way to read and construct revisions directly in memory, keyed on opaque node ids: resolve a path to a node id, list a directory's children, read a leaf's content address, mutate the in-memory revision state edit by edit, and atomically commit a new revision while advancing a branch tip — all without a working tree on disk. Where the storage system API moves content in and out of Lore in buffer form, this API moves *structure* — the merkle tree of a revision — in and out of Lore in node-id form, with content addresses as the glue between the two.

## Motivation

Lore today exposes two public API surfaces for working with repository data:

- The **storage system API** (`lore::storage`, `lore_storage_*` in `lore-capi/lore.h`) — content-addressed put / get / copy / obliterate of buffers, keyed on `(Partition, Address)`. No structure, no paths, no revisions: just bytes in and out by hash. Works against a disk-backed store or an in-memory one, with remote fetch on local miss, with no working tree involved.

- The **file-system-based revision control API** — the `lore` CLI, `lore-capi/lore.h`, and the entry points in the `lore` crate that the capi wraps. Every revision verb (stage, commit, file write, file reset, file dump, branch merge, revision sync) drives a working tree on disk: stage compares the working tree against the recorded state, commit reads the staged paths from that working tree, even reading one file out of a historical revision goes through `lore_file_dump_async` materializing bytes on disk.

There is a gap between them. The storage system API moves bytes in and out of Lore without any working tree, but only as opaque content addresses — it has no notion of paths, revisions, branches, or tree structure. The file-system-based revision control API has all of that, but only through a working tree on disk. Between buffers and a file-system materialization there is no public way to read a revision's tree by path, read a file's content address at a given revision, place a content address at a path inside a constructed revision, or commit a revision built from buffers. The merkle tree that defines a revision — nodes carrying `(name, mode, size, hash, file_id)` linked by child/parent/sibling indices, rooted in a 320-byte revision record (`docs/developing/internals/data-structures.md`) — is entirely internal. The storage system RFC named this gap explicitly: "Direct revision control over buffers is impossible. Tools cannot read or construct revisions to or from in-memory data — every revision operation must traverse the working tree" (RFC doc predates the LEP documents and cannot be referenced), and identified a path-keyed revision/merkle API as the follow-on the storage system API was designed to enable.

Automated services and pipelines using the SDK without a working tree run directly into this gap. The audience here is non-interactive: importers, mirrors, build-output ingestors, history walkers, indexers, and diff and search backends. The status quo is not slow for these callers — it is architecturally incompatible with what they do.

**Read-direction use cases.**

- A search service backend wants one file's content address at one revision so it can stream the bytes through the storage system API. Today it has to check that revision out — a full working-tree materialization just to read a single address that already exists inside the revision metadata.

- An indexer walks 1,000 revisions to compute cross-revision results: tree composition over time, file move history, content fingerprints. It cannot check out 1,000 working trees. The working-tree contract is single-tree, single-FS; the indexer's access pattern is many-revision, no-FS.

- A content service which fetches individual manifest files from specific revision from many repositories to process and drive automation tools must go through the file system to materialize individual files just to read them back into memory before it can process the data.

**Write-direction use cases.**

- An importer from a separate revision control system, or generated build outputs already has bytes and structure in hand. To register that into Lore, it has to write every leaf as a file in a working tree, run `stage`, and run `commit`. The working-tree contract assumes the working tree *is* the source of truth being committed; asking the importer to materialize its already-in-hand content as a checkout, only to have the system read that checkout back, is the wrong design, not a slow path.

- A mirror copying revisions between repositories needs to walk the source revision's tree, replicate its content into the target repository's storage, and construct an equivalent revision on the target. None of those three steps benefits from a working tree on either side, but the last one is impossible today without one on the target.

A lower-level direct revision structure API closes this gap. The new surface operates in memory on revision data — paths, nodes, content addresses, branches — with content addresses as the bridge between it and the storage system API. A pipeline loads a revision handle seeded from a parent revision (or empty), reads its tree by node id, mutates the tree edit by edit, and atomically commits the result as a new revision in the local immutable store. The storage system API handles the bytes; this new surface handles the structure; together they cover what pipelines actually need without a working tree on disk.

## Goals / Non-Goals

### Goals

1. **Read a revision tree by node id, in memory.** A pipeline loads a handle from an existing revision, resolves a path (or root) to a node id, lists a directory's children as compact records, reads a leaf's content address, and walks the tree by following node ids. Path strings are an input-side convenience for entry; iteration costs scale with node ids and the metadata required to navigate, not with reconstructed path strings.

2. **Construct and commit revisions from buffers and addresses.** A pipeline loads a handle seeded from a parent revision (or empty), applies node-level edits keyed on `parent_node_id + name`, commits the resulting revision into the local immutable store, and atomically advances a branch tip — all without a working tree.

3. **Address-level interoperability with the storage system API.** Every node-add operation accepts a `lore_address_t` returned by `lore_storage_put_async`. Every node-read operation surfaces a `lore_address_t` consumable by `lore_storage_get_async`. The address type crosses the boundary unchanged — no new identity types, no shims, no re-exports.

4. **Node-id-keyed traversal to keep memory footprint flat.** Iteration returns node ids plus the minimum metadata needed to navigate (`name`, `parent_id`, `kind`, `mode`, `size`, `address`). Full-path strings are reconstructible via a helper, not preallocated per entry.

5. **C-API shape consistent with the rest of `lore-capi`.** Sync + async variants per operation, results delivered via `lore_event_callback_config_t`, Rust entry points in the `lore` crate wrapped by the capi. No new eventing model, no new identity plumbing, no new runtime model — same patterns the storage system API and existing revision verbs use.

6. **Atomic commit + branch advance honoring existing branch semantics.** Commit through this surface uses the same branch-tip CAS, branch-protection check, tip-collision detection, and remote-write contract as the file-system-based revision API. A pipeline cannot use this API to bypass any of those guarantees.

### Non-Goals

- Replace the file-system-based revision control API. Interactive developer workflows continue to live there. This LEP adds a surface, it does not retire one.
- Expose the on-disk node block format (the 9-bit `node_index` / 23-bit `block_index` encoding from `docs/developing/internals/data-structures.md`). Node ids surfaced through this API are opaque to the caller.
- Define new authentication, partitioning, or remote-write semantics. The API inherits the trust model, identity plumbing, partition layout, and remote contract of the storage system API and the existing revision verbs.
- Add a new gRPC service or wire-protocol verb. Out-of-process and remote callers continue to use the existing revision-graph and storage RPCs; this LEP is about the in-process SDK surface and its capi.
- Provide a complete revision control surface. This LEP defines the minimum viable set needed to bridge the storage system API and the existing file-system-based revision control API — load, traverse, edit, commit, close — sized to the use cases in Motivation. Additional capabilities that pipelines may want next fall outside this LEP and land in follow-on LEPs scoped to demonstrated need, such as diff, merge and other operations.
- Extending the CLI with new low level commands. This LEP is purely for an API surface to bridge the storage and revision control gap.

## Proposed Design

The design follows the shape of the storage system API: an opaque handle, sync + async variants for every operation, results delivered via the existing event callback, Rust entry points in the `lore` crate that the capi wraps.

### Handle

`lore_revision_tree_t` — an opaque POD wrapping a monotonic `handle_id`. A revision handle is **bound to an existing storage handle** (`lore_store_t`) at load time, and to one repository within that store. The storage handle supplies everything the revision API needs at the byte level: the local immutable store (disk-backed or in-memory), the optional configured remote, the auth identity, and the runtime guard. The revision API contributes the structure on top — paths, nodes, content addresses, branch tips — without re-specifying any of that configuration.

**Repository type naming.** At the revision-control layer a repository identity is a partition (`docs/developing/internals/data-structures.md`: "In the revision control layer the partition maps to a single repository"). The revision API surfaces this identity under the name `lore_repository_id_t`, a type alias for `lore_partition_t` defined in the same style as the existing `lore_branch_id_t = lore_context_t` alias. Every revision API function signature names the argument `repository`, not `partition`; the storage handle remains repository-agnostic (one storage handle may serve multiple repositories) and continues to take `partition` per-item on its own surface.

**Function namespace.** Every new entry point lives in the `lore_revision_tree_*` sub-namespace, paired with a `lore_revision_tree_t` handle type. The namespace describes what the surface acts on — the in-memory tree of nodes that defines a revision — and keeps it visually distinct from the existing `lore_revision_*` verbs, which operate on frozen, hash-identified revisions through a working tree. The naming follows the existing capi convention of noun-based namespaces (e.g. `lore_branch_merge_*`, `lore_file_dependency_*`, `lore_repository_metadata_*`) and avoids implementation-pattern prefixes like `_handle_` that have no precedent elsewhere in `lore-capi`.

A single load entry point:

- `lore_revision_tree_load_async(store_handle, repository, revision_hash)` — loads a handle seeded from the revision identified by `revision_hash` within `repository`. A zero `revision_hash` loads an empty handle for committing an initial revision on a fresh repository. The handle is inherently mutable; loading an existing revision is not the same as "read-only mode," it is "construct a new revision derived from this parent." Pipelines that never call a write operation simply never commit new revisions.

The returned `lore_revision_tree_t` encodes the repository id internally; subsequent read, write, commit, and close calls do not repeat it.

The handle owns in-memory revision state seeded from its parent revision (or empty when loaded against hash zero) and mutated by edits applied through write operations. The implementation may flush tree blocks and revision-side fragment data through the storage handle's local immutable store at any point during the handle's lifetime; the caller sees neither the flushes nor the fragment layout. Flushes that go to remote follow the storage handle's existing remote-write contract.

**Storage handle lifecycle.** A revision handle holds its own Arc reference to the underlying `StoreInternal`, the same way two storage handle opens against the same path share an `Arc<Store>`. Closing the storage handle marks that storage handle entry invalid but does not tear the store down until every revision handle on it also closes. New API calls against a closed storage handle's revision handles still succeed against the underlying store — `lore_storage_close` does not invalidate the revision handle's view. The design phase decides whether to additionally surface a warning event when the parent storage handle is closed before its revision handles.

Concurrency, the in-flight counter, the per-handle close contract, and the "callbacks must not re-enter" rule all mirror the storage system API design.

### Read operations (Goal 1, Goal 4)

Every read operation has sync and async variants. Results are delivered as one or more events terminated by `LORE_EVENT_COMPLETE`.

- `lore_revision_tree_resolve_path(handle, path) → node_id_event` — one-shot resolution from a UTF-8 path string to a node id. Returns `LORE_ERROR_CODE_ADDRESS_NOT_FOUND` if the path does not exist in the current handle state. The only read entry point that takes a path string as input.

- `lore_revision_tree_list_children(handle, parent_node_id)` — server-streaming. Emits one `child` event per entry carrying `(node_id, name, parent_id, kind, mode, size, address)`, terminated by a list-end event. Designed for directories of arbitrary cardinality; the caller pays the cost of each entry as it arrives.

- `lore_revision_tree_node_info(handle, node_id)` — single record event with the same fields as `list_children`. The record is uniform across every node id, including the root; revision-record metadata is a separate concern (see `lore_revision_tree_info`).

- `lore_revision_tree_info(handle)` — single event carrying the loaded revision's record-level metadata: parent revision signatures, creation timestamp, author identity, metadata key count. Revision-scoped — takes no node id.

- `lore_revision_tree_node_path(handle, node_id)` — helper that walks parent pointers and assembles the full UTF-8 path string. Used for logging, display, or other cases where the caller actually needs the path; not used in hot loops. The path is relative to the **handle's own tree root** — `node_path` interprets `node_id` in the handle's `(repository, revision)` and does not follow links (mirroring `node_info`). For a node obtained by resolving *through* a link (scoped to a target `(repository, revision)`), the caller reopens a handle at that target — which `resolve_path` / `list_children` hand back — and calls `node_path` there to get the raw target-relative path. (A separate future entry point may take an explicit `(node_id, repository, revision)` and reconstruct the path in that passed-in tree without reopening a handle; out of scope here.)

Node ids are opaque to the caller. The handle guarantees a node id remains valid for the handle's lifetime; the contract after `lore_revision_tree_commit` is an open question (see Unresolved Questions).

### Write operations (Goal 2)

Every write operation has sync and async variants. Write operations mutate the handle's in-memory state directly; they do not enqueue a "stage" or "diff" intermediate.

- `lore_revision_tree_add(handle, parent_node_id, name, kind, mode, size, address) → node_id_event` — adds a leaf or empty directory under the named parent. The address is a value returned by `lore_storage_put_async`; this API does not move bytes. Returns the newly assigned node id.

- `lore_revision_tree_delete(handle, node_id)` — marks the node and any children as deleted (recursively).

- `lore_revision_tree_modify(handle, node_id, mode, size, address) → node_id_event` — Modify a node's content, only allowed on leaf nodes (files).

- `lore_revision_tree_move(handle, node_id, destination_parent_id, dst_name) → node_id_event` — moves a node between parents with optional rename (or renames within one). Preserves the source `file_id` so the revision graph records a true move.

- `lore_revision_tree_metadata_set(handle, key, value)` — sets a key on the in-progress revision's metadata. Mirrors `lore_revision_metadata_set_async` for the file-system-based API but writes into the open handle.

### Commit verb (Goal 6)

The terminal write operation to close the open work.

`lore_revision_tree_commit(handle, branch, options) → revision_hash_event` — performs the full commit revision logic, freezing the merkle tree, calculating the delta blocks, weaving history and setting the revision hash this revision was loaded from as the parent. Serializes any unflushed tree blocks and fragment payloads through the storage system API path (local always, remote when `remote_write` is set in `options`), writes the 320-byte revision record carrying `(parent revision, tree root hash, metadata root)`, and emits the resulting revision hash. Then atomically advances `branch`'s local tip to the resulting hash via the same path the file-system-based commit uses. Honors branch protection, tip-collision detection, and the existing remote-write contract — a `commit` against a protected branch fails for the same reasons a working-tree commit would. Pipelines build chains of revisions by repeatedly committing on a branch, closing the handle, and reloading against the newly-committed revision.

The `lore_revision_tree_*` sub-namespace keeps `commit` visually distinct from the existing `lore_revision_commit_async`, which is the file-system-based "commit the staged working-tree" verb. The two are semantically distinct and live at different entry points (`lore_revision_tree_commit_async` vs `lore_revision_commit_async`).

After a successful `commit`, the handle behaves as if freshly loaded from the newly-published revision: it has no pending edits, and read operations reflect the new revision. The handle remains valid until explicitly closed via `lore_revision_tree_close`.

### Address-level interop (Goal 3)

`lore_address_t` is the only identity type that crosses the boundary between this API and the storage system API. A `lore_storage_put` followed by a `lore_revision_tree_add` is the canonical "write a file" path; a `lore_revision_tree_node_info` followed by a `lore_storage_get` is the canonical "read a file" path. No new identity types, no shims, no re-exports.

### Rust + capi layering (Goal 5)

The `lore` crate's existing `revision` module (`lore/src/revision.rs`) gains the new entry points; types are imported from their original crates (`lore_base::types`, `lore_storage::store_types`, `lore_revision::event`), and `LoreRevisionTree` is the only new public type defined under the module. The capi wraps these following the existing pattern: sync + async variants per operation, results delivered via `lore_event_callback_config_t`, event tags appended to `lore_event_type_t`. The "callbacks must not re-enter the library" rule from the storage system API applies here unchanged — it is a process-wide contract that applies to all `lore_*` APIs.

### C-API usage example

The following sketch derives a new revision from an existing one by storing a file's bytes through the storage system API and adding it as a new node under the existing `docs/` directory, then commits on `main`. Each call shown is the sync variant (blocks until `LORE_EVENT_..._COMPLETE` fires). The `capture(&dest)` helper denotes a `lore_event_callback_config_t` that copies the relevant event payload into the destination pointer — full callback wiring is elided for brevity.

```c
const lore_global_args_t   globals = { /* identity, logging, etc. */ };
lore_store_t               store   = /* opened earlier via lore_storage_open */;
const lore_repository_id_t repo    = /* repository id within the store */;
const lore_hash_t          parent  = /* revision hash to derive from; zero loads an empty handle */;

lore_revision_tree_t tree;
lore_node_id_t       docs_node, new_node;
lore_address_t       addr;
lore_hash_t          new_revision;

/* 1. Load a revision tree handle bound to the storage handle. */
lore_revision_tree_load(&globals,
    &(lore_revision_tree_load_args_t){
        .store         = store,
        .repository    = repo,
        .revision_hash = parent,
    },
    capture(&tree));

/* 2. Resolve "docs" in the loaded revision to a node id. */
lore_revision_tree_resolve_path(&globals,
    &(lore_revision_tree_resolve_path_args_t){
        .handle = tree,
        .path   = { "docs", 4 },
    },
    capture(&docs_node));

/* 3. Put new file bytes into storage; receive a content address. */
const char body[] = "Hello, Lore\n";
lore_storage_put(&globals,
    &(lore_storage_put_args_t){
        .store     = store,
        .items     = (lore_storage_put_item_t[]){{
            .partition = repo,                /* partition == repository at the revision layer */
            .context   = /* fresh file_id (e.g. UUIDv7) for the new file */,
            .data      = { body, sizeof body - 1 },
        }},
        .items_len = 1,
    },
    capture(&addr));

/* 4. Add the new file as a node under "docs/" using the address from step 3. */
lore_revision_tree_add(&globals,
    &(lore_revision_tree_add_args_t){
        .handle         = tree,
        .parent_node_id = docs_node,
        .name           = { "hello.md", 8 },
        .kind           = LORE_NODE_TYPE_FILE,
        .mode           = 0644,
        .size           = sizeof body - 1,
        .address        = addr,
    },
    capture(&new_node));

/* 5. Commit on "main", atomically advancing the local branch tip. */
lore_revision_tree_commit(&globals,
    &(lore_revision_tree_commit_args_t){
        .handle  = tree,
        .branch  = { "main", 4 },
        .options = { .remote_write = 1 },
    },
    capture(&new_revision));

lore_revision_tree_close(&globals, tree);
```

The example walks the full lifecycle in one place: load (step 1) is the only entry point that takes the storage handle and repository id; every later call works against the `lore_revision_tree_t` directly. Step 3 lives in the storage system API and produces a `lore_address_t`; step 4 places that address at a path inside the revision tree without ever moving the bytes again. Step 5 atomically materializes the constructed tree as a new revision and advances the `main` branch tip.

## Compatibility

- **Wire format** — N/A. This proposal introduces no new wire messages, framings, or serialization formats. `lore_revision_tree_commit` writes the existing 320-byte revision record and the existing tree-block format defined in `docs/developing/internals/data-structures.md`, and uses the existing branch-advance and remote-write protocols (the same path the file-system-based commit uses).

- **Client/server protocols** — N/A. No new RPCs, no new message types, no new authentication flows. Commit-and-advance uses the existing branch tip protocol; flushing tree blocks and fragment payloads uses the existing storage protocol via `lore::storage`.

- **On-disk format** — N/A. The 320-byte revision record, the 64KiB tree block layout, the BLAKE3-hashed content-addressed fragments, and the partition / context plumbing all stay as-is. The new surface only writes data that the file-system-based path already writes today.

- **CLI and public API** — Additive. New `lore_revision_*` entry points appear in `lore-capi/lore.h`: `lore_revision_tree_load_async`, `lore_revision_tree_resolve_path_async`, `lore_revision_tree_list_children_async`, `lore_revision_tree_node_info_async`, `lore_revision_tree_info_async`, `lore_revision_tree_node_path_async`, `lore_revision_tree_add_async`, `lore_revision_tree_delete_async`, `lore_revision_tree_modify_async`, `lore_revision_tree_move_async`, `lore_revision_tree_metadata_set_async`, `lore_revision_tree_commit_async`, `lore_revision_tree_close_async`, plus the sync variants of each. One new type alias is introduced (`lore_repository_id_t = lore_partition_t`); the underlying partition type is unchanged. New event tags are appended to `lore_event_type_t`. No existing entry point's signature, semantics, or event tag changes. The `lore` CLI gains no new subcommands in this proposal (a future LEP could add a low-level CLI surface; out of scope here). Existing scripts and integrations are unaffected.

## Non-Functional Considerations

- **Concurrency** — A handle is `Send + Sync`; concurrent reads and writes on the same handle from multiple threads are safe and do not require external synchronization (matching the storage system API contract). Concurrent `commit` calls on the same handle are serialized by the handle's in-flight machinery; concurrent `commit` against the same branch from different handles (or from this API and from the file-system-based commit) are resolved by the existing branch-tip CAS — at most one succeeds, the others fail with a defined tip-collision error.

- **Memory** — A handle's in-memory state scales with the number of pending edits plus the tree blocks actually traversed; iteration is event-streamed so a directory of arbitrary cardinality does not preallocate. The implementation can opportunistically flush tree blocks and fragment payloads to the local immutable store; a long-running handle with many edits does not have to retain unbounded in-memory state when using a disk-backed store.

- **Statelessness** — The handle is intentionally stateful for the lifetime between load and close; that statefulness is the whole point of the surface. The library-level state outside the handle (the handle registry, the underlying `Arc<Store>` for flushes) is the same state the storage system API holds. No new process-global state is introduced.

- **Determinism** — A revision's signature is a deterministic function of its parent, tree content, and metadata, unchanged by this proposal. The same parent revision, the same sequence of edits applied in the same order, and the same caller-supplied metadata produce identical revision signatures. Revision metadata typically embeds caller-controlled context (timestamps, author identity), so two pipelines that record different metadata produce different signatures from the same edits — matching the file-system-based commit path. Concurrent edits on the same handle can interleave in implementation-defined order; the handle's intermediate state is not exposed and is not part of any contract.

## Migration Plan

N/A — no breaking changes, no migration required. The new API surface is additive; the existing file-system-based revision control API and the storage system API continue to work unchanged. Pipelines opt into the new surface by calling its entry points; nothing forces or blocks the move.

## Security Considerations

This proposal preserves Lore's existing trust model. The new surface does not introduce a new trust boundary: identity, authentication, and authorization all flow through `lore_global_args_t` exactly as for the storage system API and existing revision verbs.

Commit through the new surface honors the same branch authorization the file-system-based commit honors. `lore_revision_tree_commit` checks branch protection, partition access, and remote-write permission against the caller's identity; a pipeline cannot use this API to commit to a branch the same identity could not commit to through the CLI.

A malicious caller cannot use the new surface to construct a revision that bypasses content integrity: addresses are content-addressed (BLAKE3), and the storage system API rejects payloads whose hash does not match the claimed address. Constructing a revision with an unreachable or partition-cross-claimed address is detected at commit / push, the same way it is detected for the file-system-based commit path. No new attack surface is added.

## Privacy Considerations

This proposal preserves Lore's existing privacy posture. The metadata a revision carries (author identity, timestamps, custom keys) is determined by the caller via `lore_revision_tree_metadata_set` and matches what the file-system-based commit path records — no additional collection, processing, or sharing of user data.

Node ids are opaque to the caller and do not encode identity-bearing information. Content addresses surfaced through the new read operations are the same content addresses already visible through the storage system API and the existing revision verbs.

## Risks and Assumptions

**Assumptions**

- **Assumption:** The storage system API contracts (`lore_storage_put_async`, `lore_storage_get_async`, `LoreStore` lifecycle, the partition / address / context model) remain stable. *Invalidated if:* a future LEP changes the storage system API's identity types or call shape in a way that does not preserve the surface this LEP relies on.

- **Assumption:** The existing branch-advance path — branch protection, tip-collision CAS, remote-write contract — is the correct shared mechanism for both the file-system-based commit and the new memory-based commit. *Invalidated if:* the branch-advance path turns out to embed working-tree assumptions deep enough that the new surface cannot share it without protocol changes.

**Risks**

- **Risk:** A long-running handle accumulates unbounded in-memory edits, exhausting process memory before any commit. *Mitigation:* the implementation flushes tree blocks and fragments opportunistically to the storage handle's local immutable store; the design phase establishes a bound on retained in-memory state (likely tied to the underlying store's pressure signals). Pipelines that need a hard ceiling can `commit` the current state to a branch, close the handle, and reload against the new revision to start from a clean in-memory slate.

- **Risk:** Pipelines that construct revisions programmatically can generate malformed but verifiable trees (e.g. duplicate child names under a parent, cyclic node references) that the file-system-based path cannot produce because the filesystem disallows them. *Mitigation:* `lore_revision_tree_commit` runs the same validation pass `lore-server` already applies on push (`docs/developing/internals/data-structures.md`: "All pushed revisions have been verified by the server to be valid and correct data"), and the handle-side write operations reject malformed edits at call time.

## Drawbacks

- Three revision-related API surfaces (storage system, file-system-based, memory-based) coexisting means more surface for maintainers to evolve and more judgment calls for callers picking between them.
- Node-id semantics are a new public contract: stability rules, invalidation on flush, and behavior across commit all become commitments this LEP and its downstream spec must defend.

## Alternatives Considered

### Extend the file-system-based revision control API to accept buffers

Add buffer-accepting variants of `lore_file_write_async` and the existing stage / commit verbs, so a pipeline can hand bytes to the working-tree path without writing files.

*Rejected because:* the working-tree contract is path-keyed by full path and assumes the working tree is the source of truth, so buffer variants would still force the pipeline to model its data as a filesystem layout (full path strings per leaf, stage / unstage semantics, single committed tree) — the wrong shape for cross-revision streaming, importers, and processing pipelines. Read-side problems (path-keyed lookups, traversal of multiple revisions without checkout) remain entirely unaddressed.

### Expose only a gRPC surface via the `lore.revision.v1` service split

Land the same operations as RPCs in the new `lore.revision.v1.RevisionService` defined in `context/refs/revision-service-v1-split-rfc.md`, so out-of-process callers reach them remotely.

*Rejected because:* in-process SDK callers (the audience this LEP targets) would pay an RPC round-trip per edit, which destroys the read-and-walk performance the new surface depends on. The proposal does not preclude exposing a subset of these operations as RPCs later for genuinely remote callers; that is a separate LEP and a different audience.

### Expose the internal node block format directly

Publish the 9-bit `node_index` / 23-bit `block_index` encoding from `docs/developing/internals/data-structures.md` as the node-id type, plus accessors for the raw 64KiB tree blocks, and let callers walk and edit the tree at the on-disk format level.

*Rejected because:* the on-disk format becomes a public contract, raising the bar on any future change to the block layout, child indexing, or fragment encoding to a wire-format-class compatibility cost. The opaque node-id design surfaces the same expressiveness without the lock-in.

### Wrap the existing CLI as the SDK surface

Have pipelines drive the existing `lore` CLI (working-tree-based) via subprocess or RPC, and document that as the SDK story.

*Rejected because:* the CLI requires a working tree on disk, so every pipeline still pays the working-tree cost. Subprocess overhead per edit is prohibitive for the target audience, and the CLI surface does not expose node-id-keyed traversal or buffer-driven commit even if the I/O cost were free.

## Prior Art

The split between a high-level working-tree-driven surface (porcelain) and a low-level buffer-driven surface (plumbing) is the dominant pattern in revision control systems large enough to be used by non-interactive tooling.

- **Git plumbing.** Git's [`cat-file`](https://git-scm.com/docs/git-cat-file), [`ls-tree`](https://git-scm.com/docs/git-ls-tree), [`mktree`](https://git-scm.com/docs/git-mktree), [`hash-object`](https://git-scm.com/docs/git-hash-object), [`update-index --cacheinfo`](https://git-scm.com/docs/git-update-index), [`commit-tree`](https://git-scm.com/docs/git-commit-tree), and [`update-ref`](https://git-scm.com/docs/git-update-ref) form a complete buffer-driven path from "I have bytes" to "I have a commit pointed at by a ref," entirely bypassing the working tree. This LEP's `add` / `delete` / `modify` / `move` / `commit` shape maps directly onto `update-index` / `commit-tree` / `update-ref`.

- **[libgit2](https://libgit2.org/).** libgit2 exposes a C-API treebuilder (`git_treebuilder_new` / `git_treebuilder_insert` / `git_treebuilder_write`), in-memory ODB backends, and index-free commit construction (`git_commit_create`). The shape this LEP proposes — opaque handle, node-keyed mutation, commit-to-revision — is the same shape libgit2 has settled on after a decade of being the canonical buffer-driven Git library for tools and bindings. The opaque-handle and partition-agnostic concurrency contract this LEP inherits from the storage system API is in the same family as libgit2's `git_repository` + thread-affinity model, though Lore's `Send + Sync` posture is stronger.

- **[Sapling](https://sapling-scm.com/) / EdenSCM.** Meta's Sapling exposes its tree storage through a `scmstore` API that lets services read trees and files by path or hash without a working copy. The path-resolution-once / node-keyed-thereafter shape this LEP proposes is informed by that experience.

- **[Pijul](https://pijul.org/).** Pijul's "change" model is fundamentally non-snapshot and would not directly inform a node-id-keyed surface, but its emphasis on programmatic construction (changes as first-class data) is in the same spirit as separating the buffer-driven surface from the working-tree-driven one.

- **Perforce.** Perforce's `p4` protocol distinguishes between client-side workspace-driven operations and server-side `submit`-class verbs; bulk import tooling (`p4 unload`, scripted submits) bypasses the workspace entirely. The split between this LEP's audience (pipelines) and the file-system-based revision API (interactive workflows) is similar to that split.

## Unresolved Questions

- **Tip-collision behavior on `commit`.** When the target branch has advanced since the handle was loaded, `commit` could either fail with a tip-collision error and let the caller decide, or attempt an automatic no-op-merge / fast-forward, or rebase the constructed tree onto the new tip. The file-system-based commit fails in this case today; the design phase decides whether the new surface follows that precedent or expands it with a write-only mode that ignores advancing the local branch tip

- **Surface specification details deferred to the design phase.** Several contracts on the new surface are left to the downstream spec rather than pinned down here: the exact behavior of `lore_revision_tree_close_async` (drain semantics for outstanding operations, handle invalidation timing, failure modes); the meaning of the `kind` / `mode` / `size` / `address` arguments to `lore_revision_tree_add` for non-leaf node types (DIRECTORY, LINK); the behavior of `lore_revision_tree_add` when the target name already exists under the parent (error, replace, or no-op); and the timing of address validation against the storage layer (per-edit, at commit, or never). The Goals constrain the shape these answers can take; each is a localized design choice rather than a structural decision.
