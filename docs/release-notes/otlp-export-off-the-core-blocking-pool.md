---
type: fix
---

- `lore-server`: Fixed OTLP export of metrics, traces and logs stopping while long-running work held every thread of
  the server's blocking pool. Export now connects and reconnects to the collector from the network runtime, so a busy
  server keeps sending telemetry.
- `lore-server`: A failed metric export is logged at `warn` with its error. It was logged only at `debug`, so a
  server that had lost its collector stopped sending metrics without saying so.
