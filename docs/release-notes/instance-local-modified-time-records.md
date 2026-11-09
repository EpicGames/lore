---
type: improvement
---

- `lore-revision`: An instance keeps the modification times it records for its files in `.lore/records/` instead
  of the shared mutable store, so instances on one shared store no longer load and rewrite each other's times. A
  copy of the instance's directory that keeps its files' modification times carries times that describe its
  files, so the copy's first scan skips re-reading them. An instance created by an older client still uses the
  times it kept in the store until it records new ones. SWFS and in-memory instances keep their times in the store.
