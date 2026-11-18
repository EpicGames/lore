---
type: improvement
---

- `lore-server`: A read that misses locally and asks a read replica waits up to 10 ms for a free
  request slot on the connection to that replica. When a burst of reads takes every slot and one
  frees within the wait, the read is sent to the replica rather than to the durable store. When no
  slot frees within the wait, the read moves on up to 10 ms later than before, or, in round robin,
  up to 10 ms later for each read replica it tries. Write replication still gives up on a replica
  at once when no slot is free.
