---
type: improvement
---

- `lore-revision`: A `lore stage` or `lore status` on a FUSE mount after committed files were
  removed through it, while anything was marked or staged, is faster: after 10,000 files were
  removed from one directory, about 12 times for the stage and 26 times for the status.
