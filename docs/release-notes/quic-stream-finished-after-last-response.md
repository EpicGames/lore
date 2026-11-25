---
type: fix
---

- `lore-server`: Fixed a QUIC response going missing when the client finished its side of the
  stream while the server was still handling the request. The server finishes the stream after
  its last response.
