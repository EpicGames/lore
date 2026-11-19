---
type: improvement
---

- `lore-revision`: In a FUSE virtual clone, `lore stage` and `lore status` with `--scan`,
  `--check-dirty` or `--reset` serve from the store again a file edited in place back to what the
  current revision holds, keeping the time and permissions the file shows. Later statuses no longer
  read the file to compare it, and its copy beside the instance is removed. A plain `lore status`
  leaves the file beside the instance, reads it once, and then compares it by the time it shows,
  as in an instance on disk.
