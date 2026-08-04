## Bottom line

The right abstraction is :

```text
upload bytes → receive content address
load revision tree in memory
add / modify / delete nodes
commit tree directly to a branch
```

Lore has a draft proposal called **Low-Level Memory-Based Revision Control API** that explicitly targets automated services which need to construct revisions from buffers without creating a working tree.

## What exists today

The normal Lore workflow is still fundamentally filesystem-based:

* `stage` reads the requested file from the filesystem.
* A missing filesystem path is interpreted as a deletion.
* `commit` rereads staged files from disk and writes their contents into immutable storage.
* The current commit implementation calls `repository.require_path()` and ultimately `write_from_file_with_tracker(...)`, followed by filesystem metadata reads.

That means a pathless `RepositoryContext` by itself is not sufficient. You could load the state and alter nodes, but the ordinary commit path would still expect files on disk.

However, Lore already has most of the foundations.

### Pathless repository contexts

`RepositoryContext` already supports `path: None` specifically for server-side handlers and in-memory revision-tree operations. Filesystem operations use `require_path()` and deliberately fail on these contexts. Lore also has `new_server_context`, which connects the server stores without a checkout.

### Uploading bytes without a checkout

Lore already exposes a storage API that accepts buffers and returns content addresses. Its input includes:

* Repository/partition

* File identity context

* Raw bytes

* Whether to upload remotely

### Direct revision-tree API

The repository now contains a `revision_tree` API explicitly described as a low-level, memory-based revision-control API. It has modules for:

```text
load
resolve_path
list_children
node_info
add
modify
delete
move
metadata_set
commit
close
```

The read side is implemented and has recently landed: load/close, path resolution, child listing, node info and node paths.

But the important write files—`add.rs`, `modify.rs`, `delete.rs`, and `commit.rs`—currently contain API types and documentation rather than complete implementations.

So the exact feature you need appears to be **actively under construction upstream**.

## How difficult would it be?

| Part                               |  Difficulty | Why                                                 |
| ---------------------------------- | ----------: | --------------------------------------------------- |
| Store uploaded bytes               |         Low | Already implemented through `lore_storage_put`      |
| Load a revision without checkout   |         Low | `revision_tree_load` already works                  |
| Resolve files/directories          |         Low | Read-side tree API already works                    |
| Add/modify/delete tree nodes       |  Low–medium | Internal `State` primitives already exist           |
| Create a proper revision           | Medium–high | Must avoid the current filesystem-based commit path |
| Branch advancement and concurrency |      Medium | Existing branch semantics can be reused             |
| HTTP API around it                 |         Low | Thin Axum handler once the library API works        |

The hard part is **not editing the tree**. Lore already has internal operations capable of replacing or adding a node using its `address`, `size`, and `mode`, and direct deletion logic that marks a subtree deleted.

The hard part is constructing and publishing the final revision without entering `commit_file()`, because `commit_file()` hashes and stores the file from the filesystem even if the node already has a valid uploaded address.

The new proposal solves this by making content storage and tree mutation separate:

```text
lore_storage_put(bytes)
        ↓
Address { hash, context }
        ↓
revision_tree_add / modify(address)
        ↓
revision_tree_commit(branch)
```

That exact sequence is documented as the canonical no-working-tree write path.

## What the web API should look like

Internally:

```text
1. Read the current branch tip.
2. Verify it equals baseRevision.
3. Open the storage handle.
4. Load baseRevision into a revision-tree handle.
5. For every put:
   a. Upload bytes through storage_put.
   b. Resolve the existing node or parent directory.
   c. Modify the node or add a new one.
6. For every delete:
   a. Resolve the node.
   b. Delete it from the in-memory tree.
7. Set commit metadata.
8. Commit the tree and atomically advance the branch.
9. Close the handle.
10. Return the new revision hash.
```

For modifying an existing file, preserve its existing file identity—the address `context`—while changing the content hash. Lore’s proposed `modify` operation explicitly preserves that identity. New files should receive a fresh context, such as a UUIDv7.

Using `baseRevision` is important. The existing commit path already checks whether another instance has advanced the branch and rejects stale work with `BranchAdvanced`; your web endpoint should expose that as an HTTP conflict rather than silently overwriting changes.

### Best long-term path

Build against the new `revision_tree` API and implement the missing write verbs in your Lore fork if you need them before upstream finishes.

The likely implementation work is:

```text
revision_tree/add.rs
revision_tree/modify.rs
revision_tree/delete.rs
revision_tree/move_node.rs
revision_tree/metadata_set.rs
revision_tree/commit.rs
```

For add/delete/modify, reuse or extract logic from:

```text
stage_single_node
stage_delete
State::node_add
State::node_mark
State::node_discard_*
```

For commit, do not adapt the filesystem walker. Create an address-backed commit path that:

* Validates referenced content addresses.
* Clears temporary change flags.
* Rehashes affected directories.
* Generates the tree delta and history.
* Serializes metadata and the state.
* Advances the branch using existing protection/CAS behavior.

The strongest conclusion here is: **Lore’s authors agree with your premise and are already implementing the exact no-checkout API you need.** The practical move is to follow or finish that API, then place a small atomic changeset endpoint over it.


**Code directory:**
[J:\test\lore-new-api\lore\src\revision_tree](J:\test\lore-new-api\lore\src\revision_tree)

The API is being implemented incrementally. The module currently includes:

```text
add.rs
close.rs
commit.rs
delete.rs
info.rs
list_children.rs
load.rs
metadata_get.rs
metadata_set.rs
modify.rs
move_node.rs
node_info.rs
node_path.rs
resolve_path.rs
```

### What is implemented

The read/lifecycle side has real implementations:

* `load` and `close`
* `resolve_path`
* `list_children`
* revision and node info
* `node_path`

For example, `load` creates a pathless `RepositoryContext`, deserializes the requested revision into memory, registers an opaque handle, and returns it.

### What is not implemented yet

The parts you specifically need—directly adding, modifying, deleting and committing files—are currently largely **scaffolding/API definitions**:

* `add.rs` defines the input containing parent node, name, mode, size and content address.
* `modify.rs` defines changing a leaf’s address, size and mode.
* `delete.rs` defines recursive deletion.
* `commit.rs` currently defines the commit arguments/options, but the fetched file does not yet contain the full implementation.

So it is **real active implementation**, but the write path is not finished yet.
