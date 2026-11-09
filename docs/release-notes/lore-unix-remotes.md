---
type: feature
---

- `lore-transport`: a `lore-unix://<upstream host>/<repository>` remote reaches a machine server on
  the same machine through the socket `LORE_UDS_SOCKET` names, which a container can have
  bind-mounted. Storage requests travel on one connection per process, answered in any order, and
  the client opens its sessions again on a new connection when one closes. A `lore-unix` remote
  without the variable fails with an error that names it.
- `lore-transport`: with `LORE_UDS_SOCKET` set, every gRPC channel the process opens, the
  authentication service's included, goes through the socket in plaintext, naming the host it would
  have dialed, so the process reaches only the hosts the machine server forwards to. OIDC login
  requests still go to the network. The Lore service reads the variable from its own environment,
  not from the commands it carries out.
