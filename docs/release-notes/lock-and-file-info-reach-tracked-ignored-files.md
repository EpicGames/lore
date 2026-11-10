---
type: fix
---

- `lore-revision`: Fixed `lore lock acquire`, `lock status`, `lock release` and `lock query` skipping a committed file
  the ignore file names unless `--force` was given, and `lore file info` leaving such a file out of the local and
  filtered sizes it reports. As in status and staging, the ignore file applies only to paths the tree doesn't track.
  `lore file info` measures against the staged tree, or the revision `--revision` names, so a staged ignored file
  counts toward its sizes, also below a link mount, and a named untracked file the ignore file names is left out of
  the local size.
- `lore-revision`: Fixed `lore lock query --path` without `--branch`, and `lore_lock_file_query` with an empty `branch`
  and a `path`, failing with "unsupported lock query combination". A path given without a branch is queried on the
  branch the instance is on. A query combining `--owner` with `--path`, which the server answers for no branch, is
  refused before the request is sent, with the invalid-arguments error.
