---
type: feature
---

- `lore-server`: The `server.trust_unsigned_identity` setting lets a server without `[server.auth]`
  take the caller's identity from an unsigned bearer token on gRPC, HTTP and QUIC, so locks and
  notifications name the user instead of `<unknown>`. The identity grants no permissions. The
  server refuses to start with the setting beside `[server.auth]`.
- `lore`: Against a server that advertises no authentication, the client sends an unsigned token
  naming its identity when one is set, from `--identity` or the configuration.
