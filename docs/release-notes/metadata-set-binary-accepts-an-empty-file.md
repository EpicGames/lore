---
type: fix
---

- `lore-revision`: Fixed `lore repository metadata set --binary` and `lore branch metadata set --binary` panicking the
  service with `Cannot request zero hash from store` when the file is empty, which dropped the client's connection
