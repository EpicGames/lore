---
type: feature
---

- `lore-server`: `[immutable_store.composite] read_replica_selection` chooses which read replicas a
  read that misses the local store is sent to. `fan_out`, the default, sends it to every read
  replica. `round_robin` tries read replicas one after another, from the next position in rotation:
  up to `round_robin_max_tries` of them (default 2), each for at most `round_robin_try_limit_ms`
  (default 50) before the next starts, and at once when one misses, partially matches or fails. A
  try past its limit can still answer the read. The durable store is read once every try has ended
  without an answer, or when `durable_store_delay_ms` passes. The server logs these settings at
  startup.
