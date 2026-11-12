---
type: fix
---

- `lore`: Fixed `lore_revision_tree_modify` of a file moved through the same handle turning the move into a
  modification, so the next `lore_revision_tree_commit` recorded a modification and the file's history lost the move.
  The file stays staged as moved, with its new content.
- `lore`: Fixed `lore_revision_tree_delete` of a directory holding an added directory that a file or directory was
  moved into staging the directory for deletion while failing the entry with `LORE_ERROR_CODE_INTERNAL`, which left
  the added directory below a directory staged for deletion. The entry now fails before anything is staged and
  changes nothing.
- `lore`: Fixed `lore_storage_close` and `lore_revision_tree_close` returning while a call that started at the same
  moment on the same handle still ran; the close waits for the call, or the call fails as on a closed handle.
