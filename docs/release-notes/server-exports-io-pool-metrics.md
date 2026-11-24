---
type: feature
---

- `lore-server` metrics: the `lore-io` syscall pool runs every file operation on the default `psync` backend,
  and the operations other than positional reads and writes on `uring` and `iocp`. It is read at every metrics
  collection, every `sample_interval_millis`, under `lore.io.pool`. Gauges give the queue depth, executing jobs,
  live threads and the cap. The `window_*` gauges give the peak queue depth, peak thread count and longest queue
  wait since the previous collection, making bursts shorter than the collection interval visible. The longest
  wait includes a job still queued. Counters give jobs queued, jobs started, jobs that queued with no thread idle
  and no room under the cap, submissions whose new thread the operating system refused, and total queue wait in
  seconds. In Prometheus the mean queue wait is
  `rate(lore_io_pool_queue_wait_seconds_total[5m]) / rate(lore_io_pool_started_total[5m])`.
