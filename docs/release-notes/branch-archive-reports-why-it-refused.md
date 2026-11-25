---
type: fix
---

- `lore`: Fixed `lore branch archive` (`lore_branch_archive`) of the current branch, or of a branch
  the local archive refuses, such as the default branch or a protected one, failing with
  `BranchNotFound` (87) after logging the real reason two or three times. It now fails with the
  refusal itself, such as `DeleteCurrent` (48) or `DeleteDefault` (49), reported once, with no
  `Error` event for it before `Complete`.
