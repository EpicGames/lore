---
type: fix
---

- `lore-revision`: Fixed commands that opened the same instance with in-memory stores (the
  `in_memory` global argument of the C API) at the same time each getting stores of their own, so
  that the changes one made were missing from the other and from later commands.
