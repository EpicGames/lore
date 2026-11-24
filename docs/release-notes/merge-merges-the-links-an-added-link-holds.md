---
type: fix
---

- `lore-revision`: Fixed `branch merge start` and `branch merge into` dropping the changes a
  merged branch made inside a link nested in a link the branch adds. The merge of the added
  link's repository now merges the links it holds, level by level, with a conflict marked at each
  mount for `merge start`, and refused by `merge into` unless its text merges cleanly. A row both
  branches moved inside such a link, a conflict `merge start` has no mount to mark, and a
  repository two links reach, where the branch adds one of them or the merge is `merge into`,
  are refused as not supported.
- `lore-revision`: Fixed `branch merge start` creating a branch in a linked repository for a link
  nested in a merged link, where the main repository's merge then removes the merged link.
- `lore-revision`: Fixed a push from inside a link nested in a link a merged branch adds failing
  with `branch not found` where the added link's repository had no branch to land on. The merge
  gives that branch to each repository it links, at every depth, as `branch create` does.
