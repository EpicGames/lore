---
type: improvement
---

- `lore-revision`: a file that `lore sync`, `lore branch switch`, `lore reset` or a merge writes into the working tree
  takes one stat after the write instead of two, unless it is stored as a fragment list on Linux or macOS. Its
  executable bit is set or cleared for user, group and others only where the user's bit differs from the one the
  revision holds, as `lore clone` and `lore file write` already decide it, so a file whose user bit matches keeps the
  group and other bits it has.
