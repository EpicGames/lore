---
type: fix
---

- `lore-server`: Fixed `LORE_MAX_THREADS` sizing the server's async threads as if it ran 2 network
  threads rather than one per processor, so the two together could exceed the limit.
- `lore-server`: Fixed the server counting its network threads from a processor count that can be
  lower than the one its async threads use, on Windows hosts with more than 64 processors. Both
  now use the same count.
