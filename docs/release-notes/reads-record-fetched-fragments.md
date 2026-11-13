---
type: improvement
---

- `lore`: A storage API get that fetches content from the remote writes each fragment it fetched
  to the handle's local store, marked as held by the remote: with its payload where the item sets
  `local_cache` or the fragment was written with `PayloadLocalCachePriority`, and as an entry
  without its payload otherwise. A `lore_storage_get_metadata` item the remote answers is written
  the same way, without its payload. A later put of the same content uploads none of it and asks
  the remote nothing. The local store's index gains one entry per fragment fetched, and its flush
  writes more.
- `lore-revision`: A revision read that fetches content from the remote writes each fragment it
  fetched to the local store in the same way, with its payload where the instance caches or the
  fragment holds revision state, so a later write of the same content uploads none of it.
