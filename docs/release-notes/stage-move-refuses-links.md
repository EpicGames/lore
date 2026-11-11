---
type: fix
---

- `lore-revision`: Fixed `lore stage move` into a link placing the moved path at an unrelated node of the parent's tree,
  such as its root, and leaving the bytes untracked in the mount, where a later `lore reset --purge` deleted them. A move
  into, out of or within a link, and a move of the mount itself, are refused as not supported before anything is written,
  as is `lore dirty move` or `lore dirty copy` into a new directory inside a link. A `lore stage move`, `lore dirty
  move` or `lore dirty copy` onto the repository root is refused as invalid.
