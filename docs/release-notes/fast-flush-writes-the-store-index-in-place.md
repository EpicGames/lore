---
type: feature
---

- `lore-client`: `--fast-flush` (C API: `fast_flush` in `lore_global_args_t`) has the command's flushes write the local
  stores' index in place rather than replacing each of its files through a temporary file, which is faster where
  creating files is slow, as on Windows. If the process stops or a write fails while a file is being written, the store
  drops every entry that file held, including those of revisions and branches not yet pushed. Eviction and compaction
  still replace each file they write, and `--sync-data` still syncs what is written.
