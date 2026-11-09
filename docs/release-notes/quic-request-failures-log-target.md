---
type: breaking
---

- `lore-server`: the messages a QUIC storage request logs when it fails or times out now carry the
  `lore_server::quic` target, where they carried `lore_server::quic::stream_handler`. A log filter
  that names `lore_server::quic::stream_handler` no longer selects them; name `lore_server::quic`
  instead. The container sockets log their failed requests under the same target.
