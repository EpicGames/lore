---
status: accepted
date: 2026-11-13
---

# ADR-00033: Realize on the SWFS mount what an operation leaves that the current revision does not hold

## Context and Problem Statement

An SWFS instance serves its unmodified files from Lore: when SWFS has no copy of its own, it calls `read_file`, which `MountResources` answers from the instance's current revision (`current_read_state`, loaded from the current anchor). An operation that changes a tracked file without changing the current revision, such as `lore branch merge resolve theirs`, a merge that takes the other side's change or add, a cherry-pick, or `revert --no-commit`, writes the file with `write_node` or `set_file_to_immutable_store_contents`. On SWFS both only add the path to `touched_nodes`, and thaw describes each such path from the current revision (`written_files`). SWFS then takes the revision's size for the file and, when read, fetches the revision's content. The operation's result never reaches the mount: run with every repository mounted through SWFS, 45 of the merge, resolve, cherry-pick and revert tests fail this way, on files that hold the revision's content, are empty, or are missing. A removal the revision does not make, such as resolving a conflict by taking the other side's delete, is likewise described as holding nothing, which SWFS leaves alone while the source still serves the file.

For files in the current revision, the content is available, so Lore can always provide it through `read_file`. When an operation leaves a file the current revision does not hold, such as the other side's version a merge, `resolve theirs` or a cherry-pick takes from the store, or a removal the revision does not make, describing it from the current revision leaves the wrong file on the mount. (Bytes the operation computes, such as a text merge's result, have no place in Lore once the operation ends, so ADR-00032 writes them to the mount, as a plain instance writes them to disk.)

How should an SWFS operation leave a result the current revision does not hold?

## Decision Drivers

- What an operation leaves must be what the working tree shows, on SWFS as on a plain filesystem, across later operations and service restarts.
- Writes the current revision already describes, which is every file a sync or a branch switch writes, should stay as cheap as they are: a description, with SWFS fetching the content when it is read.
- SWFS keeps what is written to the mount in its write cache, persists it, and reports it as modified, which is what a result that differs from the revision is.
- SWFS takes a write through the mount as the user's own change and keeps it over what a later thaw describes. So the SWFS paths describe what a revision holds to the thaw, and write through the mount only scratch files and an edit Lore makes on the user's behalf whose result differs from the current revision, such as a merge, resolve, cherry-pick or `revert --no-commit` result ([LEP](../../proposals/2026-10-29-swfs-through-the-filesystem-provider.md#swfs-plumbing-shared-by-every-stage), "Never write the revision's own changes through the OS").

## Considered Options

- Realize a differing result on the mount at the end of the operation, and describe only what the revision holds.
- Keep an overlay of pending results beside the current revision, which `read_file` and thaw consult, persisted across restarts and cleared as revisions change.
- Serve the mount from the staged state rather than the current revision.

## Decision Outcome

Chosen option: "realize a differing result on the mount", because it needs no second source of truth: SWFS's write cache already holds, persists and reports changes made on the mount, as it does for a user's edit and for a merge's text (ADR-00032). An overlay would duplicate that, with its own persistence and invalidation, and the staged state does not hold every result an operation leaves (an unstaged result, or one `revert --no-commit` leaves) nor only those. Each result realized this way is the working tree differing from the revision by an edit made on the user's behalf, which is what the SWFS paths write through the mount.

`SwfsOperation` records what it intends at each path it writes: a node or an address from a store, a directory, or nothing. When it finalizes, while still frozen, it loads the state thaw describes from and looks up each path it has an intent for there, comparing content addresses: no content is read, and no tree is walked except for an operation that leaves changes unwritten, such as a dependency clone (ADR-00034). A path whose content or directory the state holds (the same content address, or a directory) is described as before. Any other is realized through the OS operation: a file's content written from the store and synced, or a directory created. A realized path is then left out of the description, as a path written to disk is.

A removal of a path the new tree still serves goes to the mount, where the path is there: describing it as holding nothing would not remove what SWFS holds there. A path the new tree does not serve is removed in the thaw, which names each file the user holds below it. No form of thaw removes a directory the user made, so such a directory stays.

Until it finalizes, a file the operation wrote from a store is not on the mount. Reads of it are already answered from the store (ADR-00032's bookkeeping); a copy of it, such as the `~mine` copy a merge restart makes of the file it has just reset, is likewise written from the store rather than copied from the mount, which still holds what was there before.

### Consequences

- Good, because merge, resolve, cherry-pick and revert results reach the working tree on SWFS, and survive later operations and restarts in SWFS's write cache.
- Good, because a sync or branch switch, whose writes the new current revision holds, still only describes them.
- Neutral, because a realized result is a change SWFS reports, and the modified-file record takes it in: it does differ from the current revision until it is committed.
- Neutral, because a result the current revision does not hold is written to the mount in full at the end of the operation, as a plain instance writes it to disk, rather than fetched when read, as a file the revision holds is.
- Bad, because a command told to remove a whole tree, the user's content included (`lore reset --purge`, `lore link remove`, removing a layer with purge, a forced sync over a link or layer mount path), removes the user's files there but leaves the directories they made. No thaw removes a directory the user made: naming its parent, or the root, as a directory relists only the names the source holds. Removing them through the mount instead is intentionally unsupported, until SWFS offers a thaw entry that removes a directory.
- Neutral, because the decision rests on an operation that writes the working tree storing its new current anchor before it finalizes, as sync, commit and clone do. One that moved the anchor afterwards would have every write realized rather than described: correct, but as costly as a copy.

## More Information

Verification: under `--all-swfs`, across the merge, resolve, cherry-pick, revert and dirty-tracking smoke tests (ten files), failures fell from 76 to 31 with the realization, all 45 of those that read an operation's result from the working tree. Copying a pending file from the store made the merge, cherry-pick and revert restarts pass, and removing on the mount made `reset --purge` of an untracked directory pass.

Related: ADR-00031 (the modified-file record), ADR-00032 (bytes an operation computes are written to the mount).
