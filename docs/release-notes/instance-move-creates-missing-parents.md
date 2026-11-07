---
type: improvement
---

- `lore-revision`: `lore repository instance move` creates the directories above its target that don't exist, where it
  failed with `Target path parent does not exist`. It creates them only once every check has passed, so a dry run or a
  refused move creates nothing, and a move that fails or is undone removes them again. A target inside the instance is
  refused before anything changes, where the move failed at the rename
