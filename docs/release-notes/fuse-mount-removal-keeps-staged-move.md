---
type: fix
---

- `lore-revision`: Fixed `lore stage` of a directory on a FUSE mount dropping a staged move into a
  directory the revision holds, whose file or directory was then removed through the mount: with
  nothing else staged, `lore commit` reported nothing staged instead of committing the deletion.
  A `lore status` dropped the move from the staged state too. The move stays staged, marked dirty
  as `lore file dirty` marks a removal on an ordinary instance, and the commit deletes the file or
  directory, or leaves the move out where a directory stands at its committed path again, as an
  ordinary instance does.
- `lore-revision`: Fixed `lore status` on a FUSE mount after the target of a `lore file dirty move`
  was removed through the mount, alone or with a new directory it was moved into: the next
  `lore commit` deleted the moved file or directory although nothing was staged, and a
  `lore stage` left that deletion unstaged. A `lore status` run between the move and the removal
  lost the move too. The moved node goes back to its committed path, marked deleted there as
  `lore file dirty` marks it, or keeps its move where something else now stands at that path:
  the commit takes nothing until the deletion is staged.
- `lore-revision`: Fixed a `lore status --scan`, or any `lore status` on a FUSE mount, run after a
  `lore file dirty move` of a directory marking the moved directory as added, which lost the move
  once the directory was removed: the next `lore commit` deleted it although nothing was staged.
  A status keeps the move, on an instance on disk as on a FUSE mount.
- `lore-revision`: Fixed a file a merge staged as an add dropping out of the staged state once it
  was removed: on a FUSE mount at the first `lore status` after its removal through the mount, and
  on an ordinary instance at `lore file dirty` of its path where a `lore status --scan` had marked
  it modified before. The file stays staged with the merge: the merge commit leaves it out while
  it is removed, and on a FUSE mount records it once it is made again with the content the merge
  staged.
