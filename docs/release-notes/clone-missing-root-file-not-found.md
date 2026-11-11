---
type: breaking
---

- `lore-revision`: `lore clone --root-file` with a root file that does not exist exits with code 82 (file not found), as
  `lore sync --root-file` does, rather than 83 (node not found). `lore_repository_clone` returns 82 for it in its return
  value and in `Complete.status`. Check for 82 where a caller checked for 83.
