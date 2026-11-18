---
status: accepted
date: 2026-11-13
---

# ADR-00037: Describe an SWFS instance's link and layer changes by their mount paths

## Context and Problem Statement

An SWFS instance serves its links and layers from their own repositories through its served tree (ADR-00036). A change to one, such as `link add`, `link update`, a sync that moves a pin, `layer add` or a layer sync, changes every path below its mount path, and `link remove` and `layer remove --purge` remove them, the user's files with them, as on a plain instance.

An operation's thaw described what it wrote path by path, and realized on the mount what the current revision did not hold (ADR-00033). A link or layer change described that way cost a description, or a write or deletion on the mount, for every file below its mount path. A removal through the mount during a freeze was SWFS's own change to a name touched while frozen, which no description in the thaw ending that freeze undid, so it hid a link that came to serve the name until a later thaw.

How should an operation describe a change to a link or layer to SWFS?

## Decision Drivers

- The working tree an SWFS instance shows must be the one a plain instance would hold after the same operations, the user's changes inside a link or layer included.
- The cost of adding, repinning or removing a link or layer should not grow with the number of files under it beyond what fetching their content costs.
- SWFS changes on the SWFS team's schedule: what it does not support is asked of that team, and worked around in Lore until it lands, not assumed.
- SWFS takes a write through the mount as the user's own change and keeps it over what a later thaw describes. So the SWFS paths describe what a revision holds to the thaw, and remove what the user holds by naming it in a thaw ([LEP](../../proposals/2026-10-29-swfs-through-the-filesystem-provider.md#swfs-plumbing-shared-by-every-stage), "Never write the revision's own changes through the OS").

## Considered Options

- Describe each changed link or layer by its mount path.
- Describe, write or delete each path below it, as ADR-00033 does for any other path.

## Decision Outcome

Chosen option: "describe by the mount path", because one entry refreshes or drops everything below a mount path, at any depth, and keeps the user's changes there, so a link or layer change costs one thaw entry plus the content it fetches. Describing path by path costs an entry, or a write, per file below it, and a deletion through the mount during a freeze is not undone by the thaw that ends it.

**What a mount-path entry does.** A thaw naming a served directory makes SWFS list it again and refresh everything below it from the source, at any depth, sizes included, while keeping the user's edits, deletions and files below it. A thaw naming a directory as holding nothing drops the source's content below it in one entry, again keeping the user's changes, without SWFS listing the removal as a change. Both were determined experimentally. A file SWFS holds changed is left as it is until a thaw names the file itself, as `swfsThaw` in `swfs.h` says, so below a changed mount path such a path keeps a description of its own.

**Which links and layers are described.** The mount keeps the tree it last described to SWFS (`MountResources::described_tree`). An operation's finalize compares it with what the mount now serves (`mount_changes`) and describes each link or layer that differs, whatever moved it in between, such as a commit inside a link, which moves its pin without a thaw. An added or repinned one is named as a directory, and a removed one as holding nothing, in place of the paths below it.

**Matching what is on disk.** Where an operation moves a pin or a layer without writing what changed, ADR-00034's handling of unwritten changes applies across the mount: each changed path the operation left alone keeps what the working tree showed, its old content written to the mount, or the path recorded as absent.

**Removal.** A path the operation removes that the mount serves is removed as an intent, and so left to the description of the link or layer it lies under. Where a plain instance's removal deletes the user's files too, as `link remove` and `layer remove --purge` do, the thaw ending the operation also names each file the user holds below the mount path, as the modified-file record holds them (`name_users_files_below`), which is proportional to the user's changes, not to the tree. After the thaw, what it could not remove below the mount path, such as directories the mount made itself, is removed unless it holds a file of the user's.

**Waiting for the thaw.** `swfsThaw` returns before the thaw is done, so the operation waits for `swfsIsFrozen` to clear before acting on the mount.

### Consequences

- Good, because adding, repinning or removing a link or layer costs one thaw entry per link or layer plus the content it fetches.
- Good, because the user's changes inside a link or layer survive a repin, as on a plain instance.
- Bad, because the mount-path entries rest on SWFS behaviour its header does not document, observed with SWFS 2026.07 and to be confirmed with the SWFS team. If SWFS does not guarantee it, link and layer changes go back to per-path descriptions and deletions, at a cost per file.
- Good, because a thaw does not change the type of a path where the user holds something: SWFS skips the entry and keeps the user's node (`SWFSResult_ThawPathConflict`), so no description overwrites the user's edit there. An operation does not make a directory where the user holds a file either: as on disk, making one there fails, so a single thaw ends every operation.

## More Information

The SWFS behaviour cited above was determined experimentally, with SWFS 2026.07 and each scenario in a fresh mount, which showed that a thaw naming a directory relists it and every level SWFS listed below it, drops what the source no longer holds at any depth, and brings back a name an earlier thaw described as holding nothing.

Where a command stores a link or layer change, so that the operation describing it finds it, is decided with the code that orders those commands.

Related: ADR-00033 (results the revision does not hold are written to the mount), ADR-00034 (keeping what an operation leaves unwritten), ADR-00036 (serving links and layers through the served tree).
