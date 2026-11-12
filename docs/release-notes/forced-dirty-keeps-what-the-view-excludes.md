---
type: fix
---

- `lore-revision`: Fixed `lore --force dirty` in a sparse working tree marking the deletion of
  committed files the view leaves out. A forced dirty mark asks the view about a deletion, as
  `lore --force stage` does: for a directory or a file named, and below a directory removed from
  the working tree, it marks only the deletion of what the view keeps.
