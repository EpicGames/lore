---
status: accepted
date: 2026-11-13
---

# ADR-00035: Keep SWFS operations off the mount the process serves, rather than give mount syscalls a pool of their own

## Context and Problem Statement

Every blocking file syscall in a Lore process runs on one bounded pool, lore-io's syscall pool (`min(2 × cores, 16)` threads): opens, metadata, directory reads, and on the `psync` backend the reads and writes themselves. The store reads its packfiles through it, and so does every caller reading a working tree.

An SWFS instance's working tree is served by the same process. A syscall on the mount can wait in the kernel until SWFS calls back into Lore (`fill_dir_begin`, `read_file`, `get_file_info`), and the callback reads the store, through the same pool, before it returns.

`lore revision restore` under `--all-swfs` hung for good on this. Its commit walk ran while the modified-file record was untrusted, as it was after every revision change until a scan reseeded it, so each of the walk's files fell back to the mount: up to `MAX_CONCURRENT_TASKS` file tasks stat, open and read files there. Stacks taken from the hung service showed all sixteen pool threads blocked in `metadata`, `open` and `read_vectored_at` on mount paths, and SWFS's single callback thread in `read_file` → `block_on`, waiting on a store read queued behind them. The mount waited on the pool and the pool on the mount. Every other process touching the mount then blocked in the kernel until the service died.

Bounding the syscalls the SWFS operation makes itself (`file_info`, `read_directory`, `holds_name_exactly`, writes) below the pool's size was tried and was not enough: a fallback content source is a path (`ContentSource::File`) that lore-storage opens and reads later, on the pool, outside anything the operation holds.

How should a syscall on a mount the process serves be kept from starving the I/O that serving it needs?

## Decision Drivers

- No interleaving of a working-tree walk and SWFS callbacks may deadlock, however wide the walk.
- A plain instance, and an SWFS instance whose record is trusted, must not get slower.
- A fix inside the SWFS operation does not cover I/O on mount paths that no SWFS code issues itself, such as lore-storage reading a content source.

## Considered Options

1. **A second syscall pool for served paths.** lore-io keeps a registry of roots served by this process; a path-based syscall under one (open, metadata, directory reads) runs on a separate pool, and a file opened under one carries that pool for its reads and writes. The SWFS mount manager registers each mount path for as long as it is mounted.
2. **A priority lane for callback I/O.** Syscalls submitted while serving an SWFS callback go to the front of the pool's queue and may start a thread past the cap. Needs the callback's I/O tagged wherever it is submitted, including work the store spawns onto the runtime (`read_stream`'s defragment pipeline), so the tag has to ride the execution context through `lore_spawn`.
3. **Keep operations off the mount.** Answer an operation's reads from the store and the frozen snapshot rather than through the mount, and treat a hang from what still reaches the mount as a bug in whatever reached it.

## Decision Outcome

Chosen option: "keep operations off the mount", because it removes the hang for every operation tested without changing lore-io. An operation rebuilds an untrusted modified-file record from `swfsGetModifiedFileList` when it begins (`rebuild_untrusted_record`, ADR-00031), which SWFS keeps across thaws and remounts and which names modifications, additions and deletions, as was determined experimentally, and fails where the record cannot be rebuilt, rather than read through the mount. The operation's view then answers from the store and the frozen snapshot, so `test_restore` passes under `--all-swfs`. A path an ignore rule matches is answered as any other: the record holds every change to a path the revision tracks, and SWFS freezes every file it holds changed, ignored or not. Option 1 removes the cycle wherever the mount I/O comes from, but adds a second pool and a notion of served roots to lore-io for cases no current operation reaches. Option 2 leaves the cycle in place for any work that loses the tag.

What an operation still reads and writes through the mount, each syscall with a permit from one process-wide bound (`MountIo`, a quarter of the syscall pool's threads, at least one), is:

- the renamed files at risk, each read and written back through the mount, one at a time and before the freeze, by the operation that holds the mount (`give_renamed_files_at_risk_bytes_of_their_own`), a workaround until SWFS names a renamed file's origin;
- the files an operation writes to the mount itself, which it writes, copies and removes there, and reads back within the operation: their information, listings and names, and their content as a source read a range at a time, each range with its own permit, so lore-storage reads it within the bound too;
- the results it realizes and the old content it keeps, written to the mount, the store read within the permit;
- whether the mount holds something at a path the operation removed, as the thaw describing it is built, at the paths an operation leaves alone below what it removes, and in the directories a thaw describes again;
- what is left below a removed link or layer mount, which the operation lists and removes;
- a directory the operation moves through the mount, which it lists, and each file below it SWFS holds as the revision holds it at the path it was moved from, with no bytes of its own, which it reads whole before the move so SWFS fetches it from there (`fetch_through_mount`), and what it removed below that directory, where the revision it began at serves nothing, which it removes from the mount after the move (`remove_carried_removals`);
- a removed directory the revision still serves that the operation wrote or made something in since, which it lists and removes entry by entry, keeping what it wrote or made (`remove_tree_keeping_what_was_made`);
- any future code that walks the mount, which the bound does not cover unless it goes through the operation.

A file SWFS froze without bytes of its own, whose origin neither SWFS nor the moves the mount kept name, is not read at all: the read fails rather than going through the mount.

### Consequences

- Good, because lore-io keeps one syscall pool, and a plain instance pays nothing.
- Good, because an operation on a mounted instance reads what changed from the store and the frozen snapshot, not through the mount.
- Neutral, because the cycle is bounded: a syscall on the mount still runs on the pool that serving it needs, but no more than a quarter of the pool's threads, at least one, run such syscalls at once, so the callbacks they wait on always find threads, except on a pool of a single thread. The bound costs mount I/O its concurrency: no more of it runs at once than the bound allows, across every mount.
- Bad, because nothing enforces that new code keeps off the mount, so a change that reads a working tree directly on an SWFS instance has to be reviewed for it.

## More Information

The original reproduction, before the record was rebuilt: `uv run pytest scripts/test/test_restore.py::test_restore --all-swfs -n 1` timed out in `revision restore` after 90 seconds; a debugger attached to the `lore service run` process showed the stacks above.

Revisit with option 1 if the cases above show up in practice, or before more code reads working trees directly.

Related: ADR-00031 (the modified-file record), ADR-00033 (results the revision does not hold are written to the mount), ADR-00034 (serving the view and keeping unwritten changes).
