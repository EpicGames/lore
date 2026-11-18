---
status: accepted
date: 2026-11-13
---

# ADR-00034: Serve an SWFS instance's view, and keep what an operation leaves unwritten across a revision change

## Context and Problem Statement

A plain instance's working tree holds what operations wrote into it. A view leaves out what it excludes, because clone, sync and the view change never write it. A clone or sync restricted to a dependency set (`--root-file`) writes only the dependency-reachable files: a dependency clone leaves the rest absent, which status then reports deleted, and a dependency sync leaves the files outside the set with the content they had.

An SWFS instance's working tree is served: `MountResources` answers SWFS's `fill_dir_begin` and `read_file` callbacks from the instance's current revision, whole. Whatever a view excludes is listed and read, and when an operation moves the instance to another revision, every file the revision changes takes the new content whether the operation wrote it or not. Run with every repository mounted through SWFS, 14 smoke tests of views and dependency filters fail on files that should be absent or should hold their earlier content.

Two constraints shape the answer. SWFS keeps what is written to the mount, so content written there sticks (ADR-00033). But a path the source serves and SWFS has not yet listed cannot be hidden from the mount side: removing it finds nothing, and describing it in a thaw as holding nothing leaves it alone or turns it into an empty file, as was determined experimentally. What the mount must not show has to be left out of what Lore serves.

How should an SWFS instance honour a view, and the partial tree a dependency clone or sync leaves?

## Decision Drivers

- The working tree an SWFS instance shows must be the one a plain instance would hold after the same operations, so that a view and a dependency clone mean the same on both. What can't be made to mean the same is refused rather than approximated.
- A narrow view over a large repository must cost nothing in proportion to what it excludes.
- What hides a path must survive the service's restarts, as a plain instance's partial tree does.
- An operation that writes everything it changes, which is every unrestricted sync and every branch switch, must not pay for any of this.
- SWFS takes a write through the mount as the user's own change and keeps it over what a later thaw describes. So the SWFS paths describe what a revision holds to the thaw, and write through the mount only scratch files and an edit Lore makes on the user's behalf whose result differs from the current revision, such as the old content a moved link or layer keeps where the operation left it unwritten ([LEP](../../proposals/2026-10-29-swfs-through-the-filesystem-provider.md#swfs-plumbing-shared-by-every-stage), "Never write the revision's own changes through the OS").

## Considered Options

- Filter what Lore serves by the view, and for a revision change, keep the unwritten changes: old content written to the mount, new paths recorded as absent and left out of what Lore serves.
- Treat a view and a dependency set alike, by removing from the mount everything they leave out.
- Persist an overlay of every path's content beside the revision, which all serving consults.

## Decision Outcome

Chosen option: "filter by the view, and keep the unwritten changes", because it puts each kind of partiality where SWFS can honour it. A view is a rule over paths, so applying it to what Lore serves costs one filter check per listed entry and nothing for what it excludes; removing every excluded path from the mount would cost a deletion per path and fail for paths SWFS has not listed. A dependency operation's partiality is a set of paths, mostly ones it did not write, and content already on the mount is kept by writing it there, which ADR-00033 already does; only paths the new revision adds need hiding, and those are what the absent set records. A full overlay would duplicate what SWFS's write cache keeps.

`MountResources` holds the instance's view and leaves out of listings, reads and file information every path it excludes (`Filter::excludes` with `FilterMode::View`), and every path in the instance's absent set, which it saves in the mutable store under a per-instance key, as the modified-file record is saved, so that a later mount serves the same tree.

A sync restricted to a dependency set is refused on an SWFS instance (`FilesystemProvider::supports_dependency_sync`). Keeping what lies outside its set made the files the working tree showed match a plain instance's, but a scan afterwards reported them differently, the file kept at its old content as modified and not the file the new revision added as deleted, and a file a link's new pin removed was not kept: the user could stage or commit the wrong paths. So it is not supported, rather than reconciled with the record and the absent set.

An operation that leaves changes of the revision it moves the instance to unwritten says so (`InstanceOperationImpl::leaves_changes_unwritten`): a clone restricted to a dependency set, and any operation that fails. When such an operation's finalize finds the state thaw describes from at a different revision than the one the operation began at, it diffs the two; any other operation writes every change and diffs nothing. A link or layer that moved is diffed below its mount path whatever the operation (ADR-00037). Each changed path the operation did not write, that the view includes and that SWFS holds no change to (the modified-file record holds none), keeps what the working tree showed: a path the old revision held gets its old content written to the mount, and a path only the new revision holds joins the absent set. A path an operation writes, or creates, leaves the absent set.

The old content kept this way (`keep_unwritten_changes`, through `write_stored_to_mount`), and the directories made for it, are written through the OS: the working tree differs from the new revision there, so they are the user's edit as the user would have kept it. The view and the absent set are applied to what Lore serves only, and nothing of them is written to the mount.

### Consequences

- Good, because views and dependency clones behave on SWFS as on a plain instance.
- Good, because a view costs a filter check per listed entry, and an unrestricted sync, which writes every change, keeps nothing and diffs nothing at its end.
- Neutral, because a file kept at its old content is a change SWFS reports and the record takes in, as it is a change a plain instance's status reports.
- Bad, because a dependency clone of a large revision records every path outside its set in the absent set.
- Bad, because a sync restricted to a dependency set is refused on an SWFS instance, where a plain instance's goes through.
- Bad, because an operation that leaves changes unwritten, a dependency clone or a link or layer moving to another revision, diffs the revisions it moved between at its end to find what to keep: a walk of what changed, which a plain instance doesn't need, as its disk keeps the old content where nothing writes it.

## More Information

Verification: under `--all-swfs`, the view, filter, merge-view, view-change and dependency smoke tests (`test_view.py`, `test_filter.py`, `test_merge_view.py`, `test_view_change.py`, `test_status.py`'s view tests, `test_clone_deps.py`).

Related: ADR-00031 (the modified-file record), ADR-00033 (results the revision does not hold are written to the mount).
