---
type: fix
---

- `lore-revision`: Fixed `lore file reset` of a path inside a link undoing an unrelated dirty move
  in the parent repository, where the parent held a file marked deleted that both repositories
  numbered alike. The parent's move is kept.
- `lore-revision`: Fixed a ProjFS directory search on Windows of a path inside a link listing an
  unrelated directory of the parent repository. The search lists nothing from the parent there.
- `lore-revision`: Fixed a status scan on a FUSE-served instance, scoped to a directory the
  revision does not hold, adding a file two or more directories below it outside its directory.
