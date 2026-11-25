---
type: improvement
---

- `lore-server`: A server answering many requests at once on a QUIC stream, such as a burst of reads
  from a peer, sends the responses sooner. The network threads write all the responses waiting on
  a stream together, rather than each response waiting for the stream to be handed on to it.
- `lore-server`: A QUIC request turned away no longer stops the server reading the stream's next
  requests while earlier responses wait for the client to read them, unless many requests on the
  stream are turned away while the client is not reading.
- `lore-server`: Nightly builds reported the wait for a QUIC response's stream as
  `urc.quic.stream_handler.response.lock_wait_duration`. It is reported as
  `urc.quic.stream_handler.response.queue_duration`, the wait for the stream's writer, and the
  "Slow QUIC response" log reports it as `queue_ms` rather than `lock_wait_ms`.
