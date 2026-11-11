---
type: fix
---

- `lore-revision`: Fixed `lore clone --vfs fuse` waiting for the root's ignore file from the remote
  before it started its background prefetch. The clone starts the prefetch first, and reads the
  ignore file while it caches the revision's state.
