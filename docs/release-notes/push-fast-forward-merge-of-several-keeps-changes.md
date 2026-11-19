---
type: fix
---

- `lore-revision`: Fixed `lore push --fast-forward-merge` of several revisions leaving the branch's
  latest without the changes another instance pushed meanwhile. Each pushed revision from the
  first one the remote merges on is now merged on the remote, and one that conflicts with those
  changes fails the push, to be synced and merged locally.
