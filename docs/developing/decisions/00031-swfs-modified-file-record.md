---
status: accepted
date: 2026-11-13
---

# ADR-00031: Track an SWFS instance's modified files in a separate in-memory State

## Context and Problem Statement

An SWFS-backed instance serves its working tree from a virtual filesystem, and SWFS reports changes to Lore through `notify_write`, `notify_create`, `notify_move` and `notify_delete`. Lore registered all four and ignored them, so `lore status --scan`, staging and the dirty checks walked the whole tree through the mount to learn which files differ from the revision, although SWFS already knew.

Where should Lore keep what those callbacks report, and how should the filesystem diff use it?

## Decision Drivers

- Commands load and store the staged `State` as they always have; a callback must not need to share one persisted `State` with a running command.
- A diff must report exactly what the walk of the whole tree reports.
- Work for a change must be proportional to the number of modified files, not the size of the tree.
- A record that over-reports costs a comparison; one that under-reports hides a change.

## Considered Options

- A separate in-memory `State` per mount carrying `Dirty*` flags, read by the SWFS operation's filesystem diff.
- Write the callback results into the persisted staged `State`.
- A shared live staged `State` that commands also mutate.
- Persist the record to disk.

## Decision Outcome

Chosen option: "a separate in-memory `State` per mount", because it reuses the `State` type and the `Dirty*` flags `lore status --scan` already sets, and leaves every command's handling of the staged `State` untouched.

`ModifiedTree` (`lore-revision/src/fs/swfs/modified_tree.rs`) wraps that `State`. The callbacks mark nodes added, modified or deleted, and a move is a delete of the old path and an add of the new, as SWFS has no moves. Paths the instance's ignore rules, view filter or repository directory exclude are skipped before any work is done. A change the record cannot take in leaves it untrusted.

The instance's ignore rules are the user's own `.loreignore` where SWFS holds it changed, as a plain instance reads them from disk: read from the frozen snapshot as an operation begins, which holds the file until a thaw names it, or none where the record holds it deleted, since SWFS freezes no deletion. Otherwise they are the current revision's, read from the store. Rules that differ from those in effect leave the record untrusted, for what it skipped was decided by them.

The record is held in memory alone. A remounted instance starts with it untrusted, and the operation that follows rebuilds it from `swfsGetModifiedFileList`, which SWFS keeps across thaws and remounts, and trusts it (`ModifiedTree::rebuild`).

A thaw keeps the record up to date rather than leaving it to be rebuilt. A file a thaw names takes the source's state and comes off SWFS's list, so the record drops each path the thaw names other than as a directory, which keeps what lies below it, and moves to the revision the thaw described from (`ModifiedTree::end_thaw`). A path a callback recorded since the freeze stays, as SWFS keeps the user's version of a name touched while frozen. A thaw that does not finish, that names fewer files than the operation wrote, or that has an entry skipped for a path the user holds (`SWFSResult_ThawPathConflict`) leaves SWFS's list unknown, and the record untrusted and empty, as does a revision that moved without a thaw of this process. A sync, a commit and a reset each end in a thaw naming what they changed, so none leaves the record to be rebuilt. A commit names each file it read and, of what it removed, each deletion SWFS lists (`swfsGetModifiedFileList`), except one SWFS froze something at since, which is the user's: a file it deleted, or the nearest deleted directory above it where the user deleted the directory whole; every deletion listed at or below a directory it deleted whole, which is committed as one node, as an entry naming a deleted directory commits nothing below it; and, for each file it took in renamed, the origin SWFS lists for it. It names no removed path SWFS does not list as deleted: such an entry has nothing to clear, and SWFS makes an empty file of it. Where SWFS cannot list its deletions the commit names nothing it removed, and the thaw that follows, which lists them again, fails where it cannot either. SWFS lists a renamed file by its destination and its origin as a deletion with the same `file_index`, and a deletion no thaw names stays listed after its destination is named, where it pairs with the file's next rename as its origin, a path the revision no longer holds. SWFS then reads the renamed file's bytes from that path, which serves none. The rebuild pairs the two entries; a rename with no such deletion, which SWFS leaves out while another node holds the origin's path, is recorded as an add. An operation whose record cannot be rebuilt, because SWFS cannot list its files or a callback leaves the record untrusted as each rebuild ends, fails rather than read the working tree through the mount (ADR-00035). A record a callback leaves untrusted during an operation is diffed as it stands, as what the callback could not record came after the freeze, and the next operation rebuilds it.

A rename needs no origin to diff correctly: on the OS filesystem it looks like a deletion and an addition, and telling Lore it was a move is the user's job (`lore stage move`), so it is on an SWFS instance too. The record needs the origin only to know which path is now gone, and SWFS lists neither the origin nor the directories either side, before or after a remount, as was determined experimentally. Two ways to learn it were weighed:

1. SWFS lists a rename's origin in `swfsGetModifiedFileList`, as a deletion or as the origin path carried on the renamed entry. The rebuild then takes a rename as a deletion and an addition, as the OS walk does, and no rename ever forces a walk. This is preferred, and is to be asked of the SWFS side.
2. Lore keeps the moves `notify_move` reports in memory, across the clears of the record, and a rename whose origin it never heard of leaves the record untrusted, so the next scan walks the whole tree.

SWFS 2026.07 as built on 2026-10-30 offers option 1: a move lists the destination with `UpdateName` and the origin as a deletion with the same `file_index`, and Lore pairs them (`frozen_files::moves_in`). The origin is not listed while another node holds its path, and such a rename is recorded as an add. SWFS still reads a renamed file it holds no bytes of from the source at the origin, so once the source there changes, as when the user commits a new file at the origin, the renamed file would serve the new file's bytes. Until SWFS names the origin itself, the mount keeps the moves `notify_move` reports of nodes with no bytes of their own, a node the served tree serves unchanged at the origin or one a kept move left there, in memory and at most 4096 of them; a move of what the user made or wrote, as an editor's atomic save, is not kept. As an operation begins it forgets each kept move whose destination `swfsGetModifiedFileList` no longer names as renamed: once a thaw names a renamed directory, SWFS reads what lies below it from the source at its own paths, so what was found through it is read from there since, and is queued at risk (`renamed_at_risk`). Where something is made or moved to a kept move's origin, the destination is queued, and the next operation, before it freezes the mount, rewrites each queued file through the mount with its own bytes. A renamed file's content is read from the revision at the origin SWFS lists or a kept move names, and the read fails where neither names one (ADR-00035).

The record was first saved to the mutable store and taken back at the next mount, to stand in for a list SWFS did not then keep across a remount. With the SWFS library of 2026.07 it does, as was determined experimentally, so the saved copy was removed. After that `SwfsOperation::changes_from_filesystem_to_state` diffs only the recorded paths (`diff_filesystem_listed`), honouring the diff's intent. A directory that is new is reported by the walk of the directory above it, so the result matches the whole-tree walk.

The record is read only for a diff of the whole tree that skips ignored paths and crosses no layer or link; any other diff keeps the OS walk.

### Consequences

- Good, because `status --scan` on a mounted instance reads only the modified files once the record is rebuilt.
- Good, because no command's load or store of the staged `State` changes, and the record cannot disagree with a command's staged write.
- Good, because a sync, a commit or a reset keeps the record, which drops what its thaw names as SWFS does, so the operation after one neither rebuilds the record nor walks the tree.
- Good, because a file the user changes back to what the revision holds is named in the thaw ending the successful operation whose comparison found it so, where the state the thaw describes from holds that content and the operation did not write it, as a reset names it: SWFS and the record drop it, so it costs one comparison, not one per scan. A file renamed there is not named, as the thaw would name its origin too.
- Bad, because the moves the mount keeps are lost on a remount, so a renamed file whose origin another node holds cannot be read until it is written.
- Neutral, because a write made just before a freeze can be reported by SWFS only after the thaw. Windows holds a write's data in its file cache when the program closes the file, and the cache manager's lazy writer hands it to the filesystem, and so to SWFS, up to about a second later. SWFS counts it as a write after the freeze, so the diff reports it on the next scan. Nothing is expected to tie a file write to a Lore operation, and a test that does flushes its writes to the filesystem (`os.fsync`) first.
- Neutral, because ignore rules that change, by an edit of `.loreignore` or a revision that changes it, leave the record untrusted, and the next operation rebuilds it under them.

## More Information

SWFS's frozen list marks each entry only `ChangedWhileFrozen`; whether a file was renamed is read from the modified list (`UpdateName`).

`swfsGetModifiedFileList` keeps a change until a thaw names the file, across thaws that do not and across a remount, and names modifications, additions and deletions; a rename only by its destination, with no entry for its origin or for the directories it leaves or enters. It once dropped a change at the next thaw, which is why the record was saved: see the decision above.
