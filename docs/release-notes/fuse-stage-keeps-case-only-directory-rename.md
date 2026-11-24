---
type: fix
---

- `lore-revision`: Fixed a commit on a FUSE instance keeping a directory's old spelling after the directory was renamed in case through the mount. `lore stage` now stages the rename under `--case rename`, refuses it as a case mismatch under the default case handling, and gives the directory the repository's spelling again under `--case keep`, as `lore stage --scan` does on an ordinary instance. A path named in another case than the mount spells it is staged, reset and unstaged as the mount spells it.
- `lore-revision`: Fixed `lore stage` without `--scan` taking in files nothing marked below a new directory marked dirty. On every instance it stages a directory marked dirty as added or moved as itself, and leaves out the files below it that nothing marked; a moved directory the working tree no longer holds is staged as deleted.
- `lore-revision`: Fixed `lore stage` of two or more paths below one directory inside a link: the linked repository's revision lacked them, the commit reported nothing staged, or the stage failed with "cannot add a child to a discarded node".
