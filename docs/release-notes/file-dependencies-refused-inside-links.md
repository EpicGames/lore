---
type: fix
---

- `lore-revision`: Fixed `lore file dependency add`, `remove` and `list` on a path inside a link acting on an unrelated
  file of the parent repository. They now fail with "Operation not supported" and change nothing. A `--root-file`
  inside a link in `lore clone` or `lore sync` is skipped with a warning instead of selecting unrelated parent files,
  and the command fails with "Operation not supported" when every root file is inside a link. The
  `LORE_EVENT_DEPENDENCY_RESOLVE_BEGIN` event counts only the root files kept, and is sent once they are resolved.
- `lore-revision`: Fixed `lore sync --root-file` also updating files inside a link that were not in the dependency set.
- `lore-revision`: Fixed `lore sync --root-file` failing with "file not found" for a root file spelled as `lore clone`
  accepts it, such as `./a.txt` or `dir\a.txt`.
