---
type: improvement
---

- `lore-revision`: a composite store read that misses locally goes to the durable store as soon as every read replica
  has missed, failed or been throttled, rather than after the full `[immutable_store.composite]
  durable_store_delay_ms`. The delay only holds back a durable read while a read replica is still working on it
