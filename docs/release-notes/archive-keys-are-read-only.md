---
type: breaking
---

- `lore-revision`: `lore branch metadata set` and `lore branch metadata clear`
  (`lore_branch_metadata_set`, `lore_branch_metadata_clear`) refuse the `archived`, `restored`,
  `restoring` and `released` keys, which archives and restores now own, and Lore Server's
  `BranchMetadataSet` refuses a swap that adds, changes or removes them. A value an application
  stored under one of these names before changes what archives and restores of the branch do.
  Remove or rename it before upgrading: after the upgrade, neither command nor
  `BranchMetadataSet` can remove it.
