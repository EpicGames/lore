---
type: fix
---

- `lore-revision`: Fixed `lore file write --path` and `lore repository dump --path` on a path
  inside a link reading a node of the parent repository instead of the linked file, and
  `lore file history` on such a path listing the parent repository's history and failing with
  "Address not found". `file write` now writes the linked file's content, `repository dump` prints
  the linked node, and `file history` lists the linked file's revisions.
