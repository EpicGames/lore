---
type: breaking
---

- `lore`: `lore_storage_close` completes once the flush of the handle's stores has finished, so a process that closes
  a disk-backed handle and exits without `lore_shutdown` keeps what the handle wrote in the local store's index. The
  flush writes as `globals.sync_data` and `globals.fast_flush` ask, once the eviction and compaction passes in flight
  on the local store have given up and a verification in flight has ended. A failed flush still releases the handle,
  and close returns its error; callers that treated every error from close as a handle left open must treat this one
  as a failed flush of a released handle. Set `skip_flush` in `lore_storage_close_args_t` to release the handle
  without a flush; what the handle, and the revision trees closed while it was open, wrote that no flush has written
  is then lost once nothing holds the stores. Callers built against an earlier `lore.h` must be rebuilt, since the
  argument struct has grown.
- `lore`: `lore_revision_tree_close` of a tree whose storage handle has closed completes once the flush of the stores
  has finished, and returns the flush's error, which `LORE_EVENT_REVISION_TREE_CLOSE_COMPLETE` also reports. The tree
  is released even when the flush fails. `skip_flush` in `lore_revision_tree_close_args_t` closes the tree without the
  flush. Callers built against an earlier `lore.h` must be rebuilt, since the argument struct has grown.
- `lore`: a client and a running Lore service from either side of this release misread each other's
  `lore_storage_close` and `lore_revision_tree_close` calls. End a running Lore service's process after upgrading.
