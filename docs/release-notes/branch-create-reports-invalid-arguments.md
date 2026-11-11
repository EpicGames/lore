---
type: breaking
---

- `lore-revision`: `lore branch create` and `lore_branch_create` given a name that isn't valid, or run
  on a personal branch, which can't have children, fail with `InvalidArguments` (3), where they
  failed as an internal error (-1, exit status 255). Given a zero id, they fail with
  `InvalidArguments` as well, where they created a branch with id zero. Scripts and C API callers
  that test a create given an invalid name, or run on a personal branch, for 255 or -1 should test
  for 3 as well, and one that creates a branch with a zero id should pass another id or none.
