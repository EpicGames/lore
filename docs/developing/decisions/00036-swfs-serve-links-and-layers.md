---
status: accepted
date: 2026-11-13
---

# ADR-00036: Serve an SWFS instance's links and layers from their own repositories

## Context and Problem Statement

A link mounts a subtree of another repository at a path, recorded in the parent's revision; a layer overlays a subtree of another repository at a path, configured per instance in `.lore/layer` and recorded in no revision. On a plain instance both are files on disk: `link add`, `link update`, a sync that moves a pin, `layer add` and a layer sync write the other repository's files into the working tree through the parent's filesystem provider.

An SWFS instance's working tree is served: `MountResources` answered SWFS's `fill_dir_begin` and `read_file` callbacks from the parent's current revision, and an operation's thaw described what it wrote from that revision too (ADR-00033). Neither knew about links or layers. A link node was served as a file, so listing a link's mount path asserted in `Node::child` inside an SWFS callback and took the service down. A layer's files are not in the parent's revision, so the operation wrote each of them onto the mount, where SWFS kept them as local changes. A layer sync opened an operation inside the parent sync's operation, and SWFS refuses a second freeze while frozen (`swfs_experiment`'s `mounts` mode, M1), so the sync failed to freeze. Every link and layer smoke test was skipped under `--all-swfs`.

How should an SWFS instance serve its links and layers?

## Decision Drivers

- The working tree an SWFS instance shows must be the one a plain instance would hold after the same operations, links and layers included.
- Linked and layered files must stay virtual, as the parent's do, and not become local changes in SWFS's write cache.
- SWFS is a fixed dependency: what it does not support is worked around in Lore or asked of the SWFS team, not assumed.
- SWFS takes a write through the mount as the user's own change and keeps it over what a later thaw describes. So the SWFS paths describe what a revision holds to the thaw, and write through the mount only scratch files and an edit Lore makes on the user's behalf whose result differs from the current revision ([LEP](../../proposals/2026-10-29-swfs-through-the-filesystem-provider.md#swfs-plumbing-shared-by-every-stage), "Never write the revision's own changes through the OS").

## Considered Options

- Serve virtually: resolve every working-tree path across link nodes and layer mount paths, and serve each from its own repository.
- Realize physically: keep serving the parent's revision only, and let ADR-00033 write every linked and layered file onto the mount.
- Give each linked or layered repository an SWFS mount of its own, nested inside the instance's mount.

## Decision Outcome

Chosen option: "serve virtually", because it is the only one that keeps linked and layered files virtual within the instance's one SWFS mount. Realizing physically puts every such file in SWFS's write cache, where SWFS lists it as a local change until a thaw names it, and fetches and stores its content twice. A nested mount spends one of the roughly eight mounts the driver allows (`MountManager::MOUNT_LIMIT`) per link or layer, places one SWFS mount inside another's mount path, which nothing says SWFS supports, and splits one working tree's freeze into several that cannot be taken together.

**The served tree.** Links and layers add no SWFS mount: the instance's one mount serves them, and a link's or layer's mount path, where it is placed in the working tree, is a directory inside that mount. One resolver, `ServedTree` (`lore-revision/src/fs/swfs/served_tree.rs`), loaded with the instance's state, maps a working-tree path to the repository, state and node that serve it. Outside any link or layer that is the parent's current revision. At a link node it is the linked repository, from the link's source node, at the pin the parent's staged state holds, or the current revision's pin where nothing is staged: that is the pin `link add`, `link update`, a merge and a sync write into a plain working tree. A link nested inside a linked repository is crossed at the pin its containing revision holds. At a layer's target path it is the layer's repository at the `current` revision the layer configuration holds, from the layer's source node, in place of whatever the parent holds there, as the filesystem diff's layer mounts already treat it. A link or layer whose state cannot be read is left out, with what lies at its path.

Whatever reads what the mount serves resolves the path through the served tree, as the mount's callbacks (`get_file_info`, `read_file`, `enumerate_directory`) do, and reads content through the resolved repository's context, since each repository is its own storage partition.

**One operation per mount at a time.** SWFS refuses a freeze while the mount is frozen and does not nest them (`mounts` M1), so a caller already holding an operation passes it down rather than opening another, as `layer::sync_in_operation` allows. A nested `begin_operation` on a mount this process has frozen fails at once with an error naming the cause, instead of retrying the freeze until it times out.

### Consequences

- Good, because linked and layered files are listed and read on demand like the parent's, and stay virtual.
- Good, because the served tree is the one resolver, so nothing that reads what the mount serves can disagree with the callbacks about what a path is.
- Bad, because a link or layer whose state cannot be read serves nothing at its mount path until it can be read.

## More Information

How an operation describes a change to a link or layer to SWFS, and where a command stores that change, are decided in an ADR of their own, with the code that does them.

Related: ADR-00033 (results the revision does not hold are written to the mount).
