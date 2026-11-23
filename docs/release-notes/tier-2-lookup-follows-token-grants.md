---
type: improvement
---

- `lore-server`: On OIDC Tier 2, `RepositoryGet` and `RepositoryQuery` answer a token that carries
  a resource claim only for the repositories its grant entries match, and answer `NOT_FOUND` for
  any other. A token with an absent or empty resource claim resolves any repository, as before.
