---
type: feature
---

- `lore-server`: `[server.uds]` serves the containers of a build machine over sockets in the
  filesystem, one per container, listed in `sockets`: Unix domain sockets on Linux and macOS, and
  `AF_UNIX` sockets on Windows. A socket serves storage requests from the server's stores with the
  same checks as the public QUIC endpoint, and forwards gRPC requests unchanged to the hosts
  `[server.uds.forward]` names, refusing any other host with `PERMISSION_DENIED`. `credits`,
  `credit_bytes`, `max_connections_per_socket`, `socket_bytes` and `handler_timeout_seconds` bound
  each connection and each socket, and `socket_mode` sets who may connect. The table takes effect
  with `enabled = true`, which requires `[server.auth]`, an authorizer that grants per partition,
  at least one socket, and `forward` keys that are host names alone, without a scheme or port; an
  empty `forward` starts with a warning. The server claims each socket with a `<socket>.lock` file
  beside it, refuses a socket that another server has claimed or answers on, and closes every
  socket connection at the shutdown signal.
- `lore-server`: the server refuses to start with `LORE_UDS_SOCKET` set in its own environment,
  which would send its own gRPC into a machine server's socket.
