---
type: feature
---

- `lore-server`: QUIC responses report how long each waited for its stream's writer and how long
  it took to write, including waiting behind earlier responses on the stream, as
  `urc.quic.stream_handler.response.queue_duration` and
  `urc.quic.stream_handler.response.write_duration`. Both are labelled like the handler's
  operation duration, by opcode and outcome. Writing includes waiting for the client's
  flow-control credit and the congestion window. A response that waits
  or writes for longer than `slow_send_threshold_ms`, set in `[server.quic]` or
  `[server.quic_internal]`, is logged with the connection's congestion window and round-trip
  time, rate limited across the process. Neither endpoint logs any response unless it is set.
- `lore-server`: QUIC replication clients report how each peer request was sent and its response
  arrived, as `lore.replication.client.request.queue_to_written` (from the request being queued
  for its stream until it has been written), `lore.replication.client.response.time_to_header`
  (from the request being written until the response's header has arrived) and
  `lore.replication.client.response.header_to_last_byte`. All three are labelled by peer, locality, replica type, opcode and response status. A response from a
  same-region peer slower than `slow_response_threshold_ms` in
  `[immutable_store.composite.replica_factory]` on any of these measures is logged with the
  connection's congestion window and round-trip time, rate limited across the process. No
  response is logged unless it is set.
- `lore-transport`: A QUIC connection takes a response observer, which is told the opcode,
  status, size and timing of every response the connection reads.
- `lore-server`: Per-connection QUIC statistics count lost and sent packets while a connection is
  open, as `urc.quinn.connection.path.packets.lost` and `urc.quinn.connection.path.packets.sent`,
  rather than only when it closes.
- `lore-server`: A peer's replication connections identify their role in their user agent, as
  `replication-same-region-read`, `replication-same-region-write`, `replication-other-region-read`
  or `replication-other-region-write`. The receiving server's per-connection statistics then keep
  a peer's read and write connections apart.
