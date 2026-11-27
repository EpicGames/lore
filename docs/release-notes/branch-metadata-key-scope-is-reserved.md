---
type: breaking
---

- `lore-server`: `BranchMetadataSet`, through `urc.rpc` and `lore.revision.v1` alike, refuses a swap that adds, changes
  or removes the branch metadata key `scope`, which holds the branch's scope. A value an application stored under
  `scope` before is read as the branch's scope: a branch created off that branch takes a 16-byte binary value as its
  scope, and any other value fails the create. Remove or rename it before upgrading: after the upgrade, no metadata
  write can remove it.
- `lore-revision`: `lore branch metadata set` and `lore branch metadata clear` (`lore_branch_metadata_set`,
  `lore_branch_metadata_clear`) refuse the `scope` key, and `lore branch metadata clear` without keys keeps it.
- `lore`: `lore_revision_graph_branch_metadata_set` refuses `scope` in its entries and in the keys it removes.
