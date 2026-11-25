---
type: fix
---

- `lore-transport`: Fixed requests on a busy QUIC connection, such as a replication peer's, waiting
  seconds to be sent while the connection received responses steadily. Reading a stream's
  responses no longer holds a network thread for as long as more keep arriving, so the requests
  queued on the connection's other streams are still sent.
- `lore-server`: Fixed responses on a busy QUIC connection waiting to be sent while a client sent
  requests steadily. Reading a stream's requests no longer holds a network thread for as long as
  more keep arriving, so the responses queued on the connection's streams are still sent.
