---
type: fix
---

- `lore-revision`: Fixed `lore branch switch`, `lore sync`, `lore branch merge`, `lore revision cherry-pick` and `lore revision revert` leaving a file under a directory's old spelling on a case-sensitive file system, beside the file's new path, where the same revision renames the directory only in case and moves a file into, within or out of it, or renames a directory below it.
