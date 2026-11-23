---
type: improvement
---

- `lore-base`: A client or server gives back the memory its async runtime and file I/O pool
  threads hold without using it within seconds of their going idle, so a process left idle after
  a large operation keeps much less memory resident and committed. Large allocations also no
  longer commit far more memory than they hold, which lowers the commit charge at the peak. With
  Lore's own allocator, a process runs one more thread for this, `lore-heap-reap`, outside the
  limit `lore_set_thread_limit` sets.
