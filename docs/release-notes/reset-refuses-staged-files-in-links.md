---
type: fix
---

- `lore-revision`: Fixed `lore reset` of a staged file inside a link, or of a directory inside a link holding one,
  discarding the staged edit. It refuses with `Failed to reset staged node`, as it does outside a link.
