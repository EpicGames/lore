---
type: improvement
---

- `lore-revision`: a sync that adds or deletes a whole directory walks it on every core. On a 5-million-entry delete
  the diff takes 13 s instead of 73 s, at the same memory
