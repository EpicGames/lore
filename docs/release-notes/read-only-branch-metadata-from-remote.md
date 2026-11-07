---
type: fix
---

- `lore-revision`: Fixed a read-only command going on without the metadata of a branch whose local
  copy of the metadata the instance cannot read, while the remote holds readable metadata.
  `lore branch diff` and `lore repository instance list` showed no name for such a branch. The
  command reads the remote's metadata.
