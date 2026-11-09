---
type: improvement
---

- `lore-server`: the local immutable store now uses each group's delayed flush to write index buckets changed by a
  capacity eviction pass. These writes are spread across one `flush_delay_seconds` interval instead of happening for
  every group at once when the pass ends. A pass that evicts from every group no longer queues all of their bucket
  writes on the I/O pool at once, ahead of payload reads and writes. If the server stops before a group's buckets are
  written, the entries evicted from them are back when it restarts. Servers in maintenance mode still write when the
  pass ends.
