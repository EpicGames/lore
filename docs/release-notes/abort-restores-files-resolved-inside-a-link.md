---
type: fix
---

- `lore-revision`: Fixed `lore branch merge abort`, with or without `--link`, keeping a file
  resolved inside a link and its `~base`, `~mine` and `~theirs` copies. Abort restores the file as
  it was before the merge whether or not its conflict was resolved.
