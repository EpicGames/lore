---
type: fix
---

- `lore-revision`: Fixed `lore commit` dropping a file that `lore file dirty move` moved into a new directory only
  marked dirty: the commit took in the move of a file not staged, and discarded a staged one with the directory. A
  commit takes only staged changes: a moved file not staged stays where it was committed, the move stays marked dirty
  after the commit, and a commit with nothing else staged finds nothing to commit. A directory moved with
  `lore file dirty move`, or added and only marked dirty, that holds a staged change, or held one when it was staged,
  is committed where it stands, with its committed contents and its staged changes. Where a staged change displaces a
  move not staged, such as a new file staged at the moved file's committed path or the deletion of its directory, the
  commit warns that it does not keep the move, and the file at the moved path is marked dirty as added.
- `lore-revision`: Fixed `lore commit` dropping a committed empty directory that was only marked dirty, such as one
  holding a new file marked dirty but not staged, or one marked dirty as deleted.
- `lore-revision`: Fixed `lore file dirty` marking a file moved with `lore file dirty move` as modified, losing its
  move, where the revision holds another file at its new path.
- `lore-revision`: Fixed `lore file dirty` of a directory marking a node that `lore file dirty move` moved out of it,
  or renamed in it, as deleted, losing the move.
- `lore-revision`: Fixed `lore file dirty` of a file that `lore file dirty move` renamed to its own name in another
  case marking it modified, losing the rename, which a commit then took though it was not staged.
- `lore-revision`: Fixed `lore file dirty` of an edited file that `lore file dirty move` moved leaving it marked as
  moved alone, and `lore file dirty move` of a file marked modified dropping that mark: such a file is marked moved
  and modified.
- `lore-revision`: Fixed `lore stage` of a path inside a directory moved with `lore file dirty move` staging the
  directory as an added one, after which `lore commit` marked every file in it modified. Such a stage stages the move
  of the whole directory, as `lore stage move` does. A change of a file's own inside it stays dirty until it is
  staged: an edit marked dirty, a move or rename, an add or a deletion. Past the commit, the files moved with the
  directory carry no mark, and `lore status` lists only those changes.
- `lore-revision`: Fixed `lore commit` failing with a non-zero exit status after it stored the revision, where marking
  the dirty changes again or carrying a merge's dirty changes failed. It warns and exits with status 0, and the
  changes it could not mark stay unmarked until `lore status --scan` finds them.
- `lore-revision`: Fixed `lore commit` dropping a committed file that `lore file dirty move` moved into a directory
  only marked dirty as added, once `lore file dirty move` moved that directory too.
- `lore-revision`: Fixed `lore commit` storing a revision whose directory addresses did not match what they hold:
  where a staged file had been moved with `lore file dirty move` out of a directory nothing staged, and where a
  directory only marked dirty as added had lost that mark, as marking or staging the removal of the file added in it
  took it off. Such a directory keeps its mark once emptied, and `lore reset` of the file takes it out of the staged
  tree with the file.
- `lore-revision`: Fixed `lore file dirty`, and the dirty marks a sync, switch or reset carries over, adding a
  directory below a file in the staged tree for a path below a committed file that the working tree holds as a
  directory. Such a path is not marked. For each tree it marks, the command warns once, naming the first such path and
  the file and counting the others.
