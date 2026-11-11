---
type: fix
---

- `lore-revision`: Fixed `lore sync`, `lore branch switch`, `lore link update`, `lore branch merge`,
  `lore revision cherry-pick`, `lore revision revert` and the sync of a layer overwriting a local edit to a file the
  incoming revision moves and modifies, and carrying a local edit to a file it only moves to the new path. An edit at
  the path a move takes a file from refuses the move, as an edit at the path it moves to does.
