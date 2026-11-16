---
type: improvement
---

- `lore-revision`: a FUSE virtual clone reports, for a file served from the store, the modification time its own records
  in `.lore/records/` hold for the path, which its syncs, switches and commits record, and no longer reads or writes
  times in the shared store. A committed file keeps the time it showed across a remount, also after the
  service stops abruptly right after the commit, and
  `lore repository instance clone` carries the times to the new instance. A virtual clone's commands tell whether a file
  changed by its content alone, never by a recorded time. A virtual clone made by v0.14.0 keeps its times, which v0.14.0
  recorded in `.lore/records/` too. After a downgrade, v0.14.0 reports older times, or the virtual clone's creation time, for the
  files this client's syncs and switches changed or its resets served from the store again, so a build tool that compares times can skip a rebuild those files
  need: run a clean build in each virtual clone after a downgrade.
