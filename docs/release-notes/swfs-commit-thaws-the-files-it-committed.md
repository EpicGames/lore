---
type: fix
---

- `lore-revision`: Fixed a commit on an SWFS instance leaving the files it committed held as modified, so a file deleted,
  committed, and put back by a later sync was not restored on the mount.
- `lore-revision`: Fixed a file on an SWFS instance reading back as zero bytes when it was renamed again after a commit
  of its rename.
- `lore-revision`: Fixed a commit on an SWFS instance leaving an empty file where it removed a file the mount did not
  hold as deleted.
- `lore-revision`: Fixed a directory deleted whole and committed on an SWFS instance not coming back when a later
  operation, such as a branch switch, restored it.
- `lore-revision`: Fixed a file or directory deleted on an SWFS instance after it was staged, and so removed by the
  commit, not coming back when a later operation restored it.
