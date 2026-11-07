---
type: fix
---

- `lore-revision`: Fixed `lore status` reporting a branch as diverged from the remote, with the remote
  ahead, after a `lore sync` or `lore commit` that committed a merge of the remote latest was killed
  while writing its result. The branch is reported ahead of the remote and not behind it, as it is
  after a merge commit that runs to its end.
