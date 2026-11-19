---
type: fix
---

- `lore-revision`: Fixed `lore reset` of a file staged inside a link restoring the committed content and reporting
  success, which lost the staged edit. It now fails with `Failed to reset staged node` and keeps the edit, as it does
  outside a link
