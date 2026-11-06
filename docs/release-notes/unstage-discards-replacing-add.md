---
type: fix
---

- `lore-revision`: Fixed `lore unstage` leaving a path replaced by one of the other kind, a directory over a file or a
  file over a directory, reported as `V entry -> entry`, after which `lore stage entry` committed both the file and the
  directory. Unstaging discards the added node, so the path returns to its never-staged state.
