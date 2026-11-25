---
type: improvement
---

- `lore-server`: the `Lore server error response` log line carries the gRPC status message in
  `rpc_status_message`, so a request the server could not decode can be told apart from a fault
  in the server.
- `lore-server`: the internal gRPC server, which serves forwarded requests and replication, logs
  `Lore server error response` too, so its server errors carry their status message. A forwarded
  request that fails with a server error shows its message in the logs of both servers, under the
  same `correlation_id`.
