---
type: fix
---

- `lore-revision`: Fixed a same-size edit on a FUSE mount, made right after a sync or branch switch changed the
  file, being missed by `lore stage` and `lore status`.
