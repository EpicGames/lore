---
status: accepted
date: 2026-11-13
---

# ADR-00032: Write the bytes of a merge to the SWFS mount, not to the operation's described nodes

## Context and Problem Statement

A text merge resolves to bytes Lore holds only in memory: the merged text, with conflict markers where lines conflict (`merge3_text_in_operation` in `lore-revision/src/merge.rs`). It writes them with `InstanceOperation::write_file`, whose contract is to write raw bytes, as they are, to the file system the operation was opened on. Nothing puts them in the immutable store at that point. A merge spans several Lore API calls (merge, then edits, resolve, stage, commit), and the user may edit the file between them, so the working tree is where the merge must live. Each later call reads the file back to stage or commit it (`commit.rs`, `file/hash.rs`, through `content_source`).

On an SWFS instance `SwfsOperation::write_file` discarded the bytes: it only added the path to `touched_nodes`. Thaw describes those paths to SWFS from the repository state after the operation (`SwfsInterface::thaw`), and a merged file is in no state. The file on the mount kept its pre-merge contents, which a later stage or commit would read back as the merge result. A clean text merge would silently lose the other side's changes, and a conflicted one would lack its markers. The sidecars the same merge writes with `write_node` were not the problem for writing, since they come from the store. They were unreadable in the same operation, which has been fixed separately.

`write_file` has two callers outside the providers, both in conflict realization: the merged text (`merge.rs`, `merge3_text_in_operation`, whose only production caller is `realize.rs`), and the empty `~base` a merge leaves beside a file where the revision holds no base (`realize.rs`, ignoring the result). Every other write an operation makes is described from the repository's state: `write_node` and `set_file_to_immutable_store_contents` take content from the immutable store, and `create_dir_all`, `remove_recursive` and `make_executable` carry none. `copy_file` and `remove` and `rename` already act on the mount.

How should an SWFS operation write bytes that come from the caller?

## Decision Drivers

- The merge result must survive across Lore API calls and user edits, so it must live in the working tree.
- A write must not be reported as kept when it was not.
- SWFS tells Lore of writes through `notify_*` callbacks, and serves what changed since the last thaw through `swfsReadFrozenFile`.
- Nothing may be written to the mount in a way that thaw then clears.
- SWFS takes a write through the mount as the user's own change: it holds it, reports it as modified, and keeps it over what a later thaw describes. So the SWFS paths describe what a revision holds to the thaw, and write through the mount only scratch files and an edit Lore makes on the user's behalf whose result differs from the current revision, as a merge's result does ([LEP](../../proposals/2026-10-29-swfs-through-the-filesystem-provider.md#swfs-plumbing-shared-by-every-stage), "Never write the revision's own changes through the OS").

## Considered Options

- Honor the existing `write_file` contract: write the bytes to the mount through the OS operation, and leave the path out of `touched_nodes`.
- Add a variant of `write_file`, or an argument to it, that means "to the OS", and keep the present `write_file` for paths described from the state.
- Put the merged bytes in the immutable store, stage a node for them, and let thaw describe the file.
- Fail `write_file` for non-empty content, so a text merge falls back to a binary conflict (the stopgap in place now).

## Decision Outcome

Chosen option: "honor the existing `write_file` contract", because the trait already says it writes to the file system the operation was opened on, and every caller (the merged text, and the empty base a merge leaves where the revision holds none) means that. A separate variant would add a name for what the one method is documented to do; `write_node` and `set_file_to_immutable_store_contents` are already the methods that describe content from the store. If a caller later needs bytes described from a state, that is the case that gets the new name.

The merged text is an edit made on the user's behalf, and writing it through the OS is how the user would make it, so SWFS holds it as the user's change, and it is read, staged and committed as one.

`SwfsOperation::write_file` writes through `self.os` and records the path as written to disk by this operation, not in `touched_nodes`. Thaw then does not describe it from the state, so SWFS keeps the write in its write cache.

Reads of a path this operation wrote to disk go to the OS operation, which sees the write at once, and so does a listing of the directory holding it. This replaces the check that a path is untracked: a merge writes its result to a path the revision tracks, and the view would otherwise answer from the revision. `copy_file` writes its copy to the mount in the same way, taking the same bookkeeping instead of `touched_nodes`: it reads the source as the operation reads it, from the store, the frozen snapshot or what the operation wrote, and writes it a window at a time, so a copy holds no more memory than one window whatever the file's size. A later `write_node` of the same path takes it back out of the set.

`remove` describes a path to thaw only where the revision holds it. Thaw describes a removed path as holding nothing, which hides what the revision holds there. For a path it does not hold, such as a `~mine` copy removed in a later operation by `unlink_merge_artifacts`, the path is not named: SWFS takes an entry naming a path as holding nothing as an empty file, wherever it holds no deletion there for the entry to clear, which once left an empty `~mine` behind after a merge resolved cleanly.

The two callers change as follows. The merged text is kept, as above. The empty `~base` is now created on the mount, as the OS provider creates it, where SWFS created nothing before; `exist_merge_artifacts` (an OS read through `untracked_file_info`) then reports it and `unlink_merge_artifacts` removes it, as on any other provider.

The record learns of the write from `notify_write` after the thaw, as for any user write.

### Consequences

- Good, because a text merge keeps its result under SWFS, and the user can edit the file before resolving.
- Good, because it removes a special case: `write_file` does what its contract says on every provider.
- Neutral, because the write reaches later operations through SWFS's write cache: each later freeze holds it, until a thaw names it, and it is read with `swfsReadFrozenFile`, so the status and commit that follow a merge read its result.
- Neutral, because what an operation writes to the mount itself (such as a text merge's result and its `~mine` copy) is read back from the mount until the operation ends, as a plain instance reads back what it writes to disk: neither the frozen snapshot nor the store holds those bytes yet. The `~theirs` and `~base` copies come from the store and are read from it.
- Neutral, because a write made while frozen to a path the thaw also describes, such as a tracked path written by `write_node` earlier in the operation, is kept: a name touched while frozen keeps SWFS's version (`swfsThaw` in `swfs.h`).

## More Information

Verification: `test_swfs_a_conflicting_merge_writes_conflict_markers_to_the_file` in `scripts/test/test_swfs.py` was a strict xfail for this gap and passes with this change; a clean text merge keeps both sides' changes; and a merge with no base leaves an empty `~base`. The error `write_file` returned for non-empty content is removed with this change.

Related: ADR-00031 (the modified-file record).
