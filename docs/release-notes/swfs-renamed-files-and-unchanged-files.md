---
type: fix
---

- `lore-revision`: Fixed a file on an SWFS instance that was renamed, then had a new file made at its old path, taking
  that new file's content once it was committed.
- `lore-revision`: A file on an SWFS instance written back to what the revision holds is no longer compared on every
  `lore status --scan`.
- `lore-revision`: An editor's atomic save on an SWFS instance no longer rewrites the saved file through the mount at
  the next command.
