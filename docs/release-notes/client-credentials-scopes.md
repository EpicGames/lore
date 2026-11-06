---
type: feature
---

- `lore-server`: `[server.auth.oidc] client_credentials_scopes` names the scopes clients request
  when they log in with the client credentials grant, for a provider that adds the claims Lore
  reads only for a requested scope.
- `lore-transport`: `lore auth login --client-id` requests the scopes the server names in
  `client_credentials_scopes`, and no scope when it names none. Older clients request no scope
  whatever the server names, so CI logins against such a provider need this client.
