---
type: fix
---

- `lore-transport`: Fixed OIDC discovery aborting the process with a stack overflow when the
  issuer answered with a redirect to a `blob:` URL nested many levels deep (RUSTSEC-2026-0592).
  Discovery fails as a redirect to another origin.
