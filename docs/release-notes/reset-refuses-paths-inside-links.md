---
type: fix
---

- `lore-revision`: Fixed `lore reset` of a file inside a link that the target revision does not hold, such as
  a file staged or marked dirty as an add, deleting the file from disk, discarding an unrelated dirty add in
  the parent, or refusing for the wrong reason. It fails with `Operation not supported` and leaves the file and
  its staged state as they were
- `lore-revision`: Fixed `lore reset --last-merged-from` of a file inside a link following the history of an
  unrelated parent file and resetting the file to the wrong revision. It fails with `Operation not supported`
