---
type: fix
---

- `lore-revision`: Fixed a file missing from an SWFS instance after a sync brought it back. A sync
  that removed a file, such as `lore sync main@1 --force`, made SWFS keep the deletion as the
  user's own, so a later sync to a revision holding the file again left it absent.
