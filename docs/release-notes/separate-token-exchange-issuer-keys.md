---
type: fix
---

- `lore-server`: Fixed a server whose `[server.auth.oidc] token_exchange_issuer` names a separate
  token service refusing the partition-scoped tokens that service issues. The server finds the
  service's keys through its OIDC discovery document, and verifies a token naming the service as
  `iss` against those keys only, and every other token against the `jwt_issuer` keys only. The
  server now fetches the service's keys at startup and doesn't start if it can't, and
  `token_exchange_issuer` must be an http or https URL.
