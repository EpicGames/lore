---
type: fix
---

- `lore-revision`: Fixed `lore status --scan` dropping the conflict of a file deleted on the current branch and changed
  on the merged one. The scan took the missing file for a reverted add, so the conflict disappeared from `lore status`
  and `lore commit` completed the merge without the incoming change. The conflict now stays until it is resolved.
