---
type: fix
---

- `lore-revision`: Fixed a merge, resolve, cherry-pick or revert on an SWFS instance leaving out what the current
  revision does not hold: `resolve theirs`, the other side's changes and adds, a cherry-picked or reverted file, and the
  `~base` and `~theirs` copies of a conflict. The working tree showed the current revision's file, an empty file or none
  ([ADR-00033](../developing/decisions/00033-swfs-realize-operation-results-the-revision-does-not-hold.md))
- `lore-revision`: Fixed restarting a merge, cherry-pick or revert on an SWFS instance building the conflict from the
  edit the restart was discarding, instead of putting the file back in its conflict state
- `lore-revision`: Fixed `lore branch merge resolve mine` on an SWFS instance staging the file as added when mine
  deleted it, instead of staging the delete
- `lore-revision`: Fixed resolving a conflicted merge on an SWFS instance not taking effect: SWFS kept the conflict
  markers, and `lore commit` reported the file still in conflict
- `lore-revision`: Fixed a file that a command on an SWFS instance writes into a directory it removed earlier in the
  same command, such as a merge's copy of a conflicted file, being left out when the command then lists the directory,
  or the files it had removed there being listed instead, and a directory being made over it
- `lore-revision`: Fixed a file that a command on an SWFS instance writes and then removes, itself or with a directory
  above it, still being read, listed or in the way of a directory for the rest of the command, and staying in the
  working tree after it
- `lore-revision`: Fixed a command on an SWFS instance that removes a directory the revision still holds and then
  writes into it, such as a merge writing a conflicted file's copies, losing what it wrote there when it finished
- `lore-revision`: Fixed `lore reset` of a directory the user moved on an SWFS instance, and marked moved, leaving the
  files of the directory it put back reading as zero bytes
