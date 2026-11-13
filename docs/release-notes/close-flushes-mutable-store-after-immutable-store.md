---
type: fix
---

- `lore`: Fixed the flush of a closed storage handle, or of a revision tree closed after its storage handle, writing
  the mutable store after the immutable store's flush had failed, which could leave mutable values naming content the
  local store's index didn't record. The mutable store is flushed only once the immutable store's flush succeeds, as
  `lore_storage_flush` does.
- `lore`: Fixed a failed flush of a storage handle left open at `lore_shutdown` going unreported; it is logged as a
  warning.
