---
type: fix
---

- `lore-revision`: Fixed prune retiring a virtual instance whose external directory can't be read, as on a volume that isn't mounted or where examining it fails, with its anchors, and listing reporting it stale and the multiple-instance warnings of `lore sync` and `lore branch switch` leaving it out. It's kept and reported as active.
- `lore-revision`: Fixed service start skipping such a virtual instance without a word. It reports why, and a later start mounts it once its external directory can be read.
