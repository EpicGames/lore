---
type: improvement
---

- `lore-revision`: An operation on an SWFS instance that writes every change, such as a sync, no longer diffs the
  revisions it moved between at its end; only a clone restricted to a dependency set does.
- `lore-revision`: A sync restricted to a dependency set (`--root-file`) is refused on an SWFS-backed instance, where a
  scan afterwards would have reported the files outside the set differently than a plain instance does.
- `lore-revision`: The I/O an operation makes on an SWFS mount is bounded across every mount, so it cannot fill the
  thread pool that serving the mount needs and hang the service.
