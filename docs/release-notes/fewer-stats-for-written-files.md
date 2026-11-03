---
type: improvement
---

- `lore-revision`: a file that `lore sync`, `lore branch switch`, `lore reset` or a merge writes into the working tree
  takes one stat after the write instead of two, unless it is stored as a fragment list on Linux or macOS, and its
  executable bit is changed only when it differs from the one the revision holds.
