---
type: breaking
---

- `lore-revision`: `lore link add` exits with other codes when it refuses an add. A source path
  the linked revision does not hold exits with 82 (file not found) instead of 83. A source path
  that is not a directory, and a mount path that is a file, holds children, is already a link or is
  below a file, exit with 3 (invalid arguments) instead of 255. Scripts that check these exit codes
  have to check the new ones.
