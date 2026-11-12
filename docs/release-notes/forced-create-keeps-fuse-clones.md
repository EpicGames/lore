---
type: fix
---

- `lore-revision`: Fixed `lore repository create --force` and `lore clone --force` at the path of a mounted FUSE
  virtual clone, or a path inside one, deleting the clone's Lore directory and then failing, which left the clone
  unusable. They now fail with `RepositoryAlreadyExists` (58) before changing anything, as they already did for an
  SWFS virtual clone.
