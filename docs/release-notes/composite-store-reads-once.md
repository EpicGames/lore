---
type: improvement
---

- `lore-server`: in a composite immutable store, a read that arrives while the server writes a
  fragment it fetched from the durable tier to the local tier takes the fetched copy instead of
  fetching the fragment again.
