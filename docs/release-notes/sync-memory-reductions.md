---
type: improvement
---

- `lore-revision`: collecting a diff's changes holds no spare capacity. On a 5-million-change sync this lowers the
  memory held after the diff from 3.9 to 3.0 GiB and the peak resident size from 4.9 to 4.6 GB
- `lore-revision`: a sync's verification keeps one list of changes and filters it in place. `LORE_MEMORY_STATS=1`
  reports the bytes the global allocator holds and their peak, and what the GrowVec and node block heaps hold, at
  exit, and logs them at debug level at each phase of a sync
