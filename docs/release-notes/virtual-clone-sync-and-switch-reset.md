---
type: feature
---

- `lore-revision`: `lore sync --reset` and `lore branch switch --reset` work on a FUSE virtual clone, where they were
  refused. As on an ordinary clone, every tracked file takes the target's content, and every tracked file and
  directory its spelling, whether the target changes them or not: files deleted or moved away locally come back, and
  a directory made where a tracked file stood goes, with the files in it. Files added locally stay where the target
  adds nothing. A symbolic link standing where a tracked file stood stays, as `--force` leaves it, and status lists
  the file deleted until `lore reset <path>` replaces the link; one standing where a tracked directory stood goes,
  and the directory comes back
