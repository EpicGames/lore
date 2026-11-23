---
type: fix
---

- `lore-client`: A command given only `--access-token`, with no identity token, presents the access
  token to the repository service, so `lore repository clone <repository> --access-token <token>`
  resolves the repository's name and works without a login.
