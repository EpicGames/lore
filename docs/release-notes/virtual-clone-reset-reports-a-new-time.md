---
type: fix
---

- `lore-revision`: Fixed a `lore reset` of an edited file in a FUSE virtual clone reporting the file's
  time from before the edit, so a build tool skipped rebuilding what the edit had built. The file now
  reports the time of the reset, and keeps it across a remount. As in an instance on disk, `lore reset
  --force` gives each file it names the time of the reset.
- `lore-revision`: Fixed a file in a FUSE virtual clone that a process had open, or had looked up
  shortly before, reporting its time from before a sync, a switch or a reset that changed its content.
