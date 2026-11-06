---
type: improvement
---

- `lore-client`: a command runs one async thread fewer than the processor count, which leaves a
  processor for its 2 network threads. A command that opens several storage connections, with
  `--max-connections` or `LORE_MAX_CONNECTIONS`, runs a network thread for each of them, up to half
  of its network and async threads together, and as many fewer async threads, so it runs as many
  threads as with one connection. On 8 processors, a command runs 7 async and 2 network threads,
  and with 10 connections 5 async and 4. `lore service run`, which serves many commands at once,
  keeps its async threads and adds the network threads 10 connections get: 7 async and 4 network
  threads on 8 processors. `LORE_NET_THREADS` still sets the network thread count, with one async
  thread per processor, and `LORE_MAX_THREADS` still caps the total.
