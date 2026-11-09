---
type: feature
---

- `lore-server`: `server.upstream_connections` sets how many storage connections the server opens
  to the upstream of a `remote` immutable or mutable store, from 1 to 10. Every repository the
  server reaches there shares them, for QUIC, gRPC and socket requests alike. It defaults to 4; a
  server opened one before.
