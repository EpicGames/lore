---
type: fix
---

- `lore-storage`: Fixed content missing from a local store after a read that caches it, when
  another read was fetching the same content at the same moment: one into another local store in
  the same process, such as two virtual clones over different shared stores in one service, or one
  into the same store that does not cache it, such as a prefetch stopped before it stored the
  content. The read returned the content without storing it, so a later offline read of it failed.
- `lore-storage`: Fixed a file staying unreadable from a local store after eviction or compaction
  removed its content's payload, when that content was fetched again for another file that shares
  it.
