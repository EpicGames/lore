---
type: improvement
---

- `lore-server`: `lore.revision.v1.RevisionService/BranchList` loads up to 100 branches from the store at once instead
  of one after another, so listing a repository with hundreds of branches takes a fraction of the time. Branches are
  streamed in no particular order.
- `lore-server`: `[feature] branch_list_concurrency` sets how many branches a BranchList request loads at once. It
  defaults to 100 and must be at least 1.
