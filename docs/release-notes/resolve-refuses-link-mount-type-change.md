---
type: fix
---

- `lore-revision`: Fixed resolving a conflict between a link and a file or folder, or between a folder and a file
  inside a linked repository, committing a corrupt revision, failing with an internal error or resolving nothing. It
  now fails with `NotSupported`, naming the path and the command that aborts the merge, cherry-pick or revert.
- `lore-revision`: Fixed `lore branch merge start` merging the changes below a path where one branch added a link and
  the other a file or folder. It now reports a conflict at that path, as for a file and a folder.
- `lore-revision`: Fixed resolving a folder that holds a link, such as the whole tree, failing with an internal error.
  It now passes over a link without conflicts, and fails with `NotSupported` at a link that holds conflicts, which are
  resolved by naming the paths inside it.
- `lore-client`: Fixed a bare `lore branch merge resolve`, `lore revision cherry-pick resolve` or `lore revision revert
  resolve` printing "No conflicts resolved" before the error when the resolve is refused.
