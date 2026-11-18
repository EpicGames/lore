---
type: fix
---

- `lore-revision`: Fixed `lore reset` of a deleted file on an SWFS instance leaving an empty file, and `lore reset
  --purge` leaving an untracked directory in place
- `lore-revision`: Fixed `lore sync` of an SWFS instance whose last scan left changes failing with `Failed to freeze`
- `lore-revision`: Fixed a case-only rename, in a sync or a commit, not taking effect on an SWFS mount, which kept the
  old spelling
- `lore-revision`: Fixed a sync that shrinks a file on an SWFS instance, when nothing had listed the file since the
  clone, reading as the new content padded with NUL bytes to the old size
- `lore-revision`: Fixed `lore branch switch --force --reset` and a forced `lore sync` on an SWFS instance leaving the
  other revision's content in a file whose content differs between the revisions
