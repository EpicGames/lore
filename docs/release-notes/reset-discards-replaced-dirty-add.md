---
type: fix
---

- `lore-revision`: Fixed `lore reset` leaving a path reported as changed after it restored a directory over a file
  added in its place, or a file over a directory, that was staged and then unstaged. The reset discards the add, so
  `lore status` and `lore status --scan` report no changes.
