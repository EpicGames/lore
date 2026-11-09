---
type: improvement
---

- `lore-server`: The replicated immutable store opens `num_clients` connections to the remote server (default 2).
  Each request uses the healthy connection with the fewest requests of its kind in flight, then the fewest in total,
  so requests stuck on a stalled connection steer new ones to the others. The connections refresh at staggered times,
  and one whose reconnect fails keeps retrying until it is back. `client_message_limit`,
  `max_bandwidth_bytes_per_second` and the other client settings apply to each connection, so the default doubles the
  bandwidth and the requests in flight an edge server can use toward the remote. Set `num_clients = 1` to keep one
  connection. Client connection metrics and the new `client.inflight_requests` gauge carry a `client_slot` label.
