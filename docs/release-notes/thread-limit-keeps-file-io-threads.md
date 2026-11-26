---
type: improvement
---

- `lore-base`: A thread limit, set with `LORE_MAX_THREADS`, `--max-threads` or
  `lore_set_thread_limit`, keeps the blocking threads and a command's network threads at their
  counts, and splits the rest evenly between the async threads and the file I/O threads, and
  the network threads of `loreserver`, which asks for one per processor. No pool in the split
  gets more than it asks for: what it leaves goes evenly to the others. Each pool keeps at least
  2 threads. Under a limit too small for that, the blocking and command network threads are scaled
  down into what the split leaves. A program that embeds Lore and calls `lore_set_thread_limit`
  gets more file I/O threads and fewer async threads under the same limit. Async, blocking,
  network and file I/O threads on 192 processors, before and after:
  - a command with one storage connection: 37/2/2/3 -> 23/3/2/16 at a limit of 44, and
    56/2/2/4 -> 43/3/2/16 at 64;
  - a command with `--max-connections 6`: 37/2/2/3 -> 19/3/6/16 at 44, and 56/2/2/4 -> 39/3/6/16
    at 64;
  - `loreserver`: 20/2/20/2 -> 14/3/14/13 at 44, and 30/2/30/2 -> 23/3/22/16 at 64.
