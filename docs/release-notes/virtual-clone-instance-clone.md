---
type: feature
---

- `lore-revision`: `lore repository instance clone <target-path>` clones a FUSE instance, where it was refused, into another FUSE instance that the service mounts at the target, on the source's revision, branch and
  staged state, with its uncommitted changes and deletions. Only those changes and the instance's Lore directory are
  copied, since the tracked files come from the store. The copy shares blocks where the external root's volume clones files, and is otherwise refused unless `--allow-copy` is given. The new instance reports the
  modification times the source's syncs and switches recorded, and a write in either instance leaves the other alone.
  A target that another cloning takes meanwhile is refused before the new instance is registered. A service stopped
  while it clones, as by a crash, removes at its next start a new instance it had not registered, with the empty
  directory it made at the target, and mounts one it had registered.
