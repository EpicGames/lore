---
type: fix
---

- `lore-revision`: Fixed `--sync-data` leaving the modified-time records a command writes back to
  `.lore/records/` unsynced: the flush syncs them to disk as it syncs the stores.
