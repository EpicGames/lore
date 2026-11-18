---
type: fix
---

- `lore-revision`: Fixed `lore sync`, `lore push` and `lore clone` deleting a branch's record that
  its local latest holds revisions the remote lacks, where a commit to the branch from another
  repository instance over the same store landed while they recorded the branch in sync with the
  remote. The branch then read as in sync with the remote while its local latest was a revision the
  remote lacks. They now keep the record a commit stores after their own write of the local latest.
