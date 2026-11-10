---
type: feature
---

- `lore-client`: `lore repository instance clone <target-path>` clones an instance on a shared store to a new path
  as an instance of its own, on the source's revision, branch and staged state, with its uncommitted changes. On
  the source's volume, on a filesystem that clones files (APFS, Btrfs, XFS, bcachefs, ReFS and Dev Drive), the
  clone shares the source's data blocks and takes little space. Elsewhere the clone is refused with what a copy
  would cost, unless `--allow-copy` is given and the destination has room. `--dry-run` prints how the clone would
  be made. The clone's first `lore status --scan` hashes only the files a scan of the source would: those with no
  modified-time record, and those changed since theirs. While the clone runs, commands that open the store, on the
  source instance and from other processes on every instance over the store, wait for it. So does
  `lore repository instance delete`, which opens the store to deregister the source before it removes any file.
  `lore repository instance move` renames the source before it opens the store, so moving the source while it
  copies makes the clone fail; on Windows the move is refused while the clone lists a directory of the source. A clone
  that fails removes what it copied before the command returns. A repository nested inside the instance is left
  out. A destination that exists or lies inside an instance, the source included, is refused, and so are instances
  with a local store and virtual clones. A clone that is interrupted can leave a directory beside the destination,
  which the next clone to the same destination removes.
- `lore`: the C API adds `lore_repository_instance_clone` and its `_async` form, taking a
  `lore_repository_instance_clone_args_t` with a `target_path` and an `allow_copy` flag, and the
  `LORE_EVENT_REPOSITORY_INSTANCE_CLONE_BEGIN`, `_PROGRESS` and `_COMPLETE` events. Failures report code 9 (not
  supported), 3 (invalid arguments), 4 (invalid path), 118 (oversized) or 82 (file not found), as `lore.h`
  describes for each case.
