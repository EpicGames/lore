---
type: fix
---

- `lore-revision`: Fixed `lore reset` keeping the working tree's spelling of a file, a directory or a link's mount
  that it holds in another case than the revision does. On a file system that does not fold case, a reset that reached
  the entry through its directory wrote the revision's spelling beside it, and on one that folds case it left the
  entry spelled as it was. A reset renames the entry to the revision's spelling and restores its content, whether it
  reaches the entry through a directory or names it, below a layer's mount too. It renames an entry only where it is
  the only entry of that name, and of the same kind as the revision's. Where the file system refuses the rename, or a
  layer is mounted at or below the entry, the reset restores the entry where it stands and warns once, with how many
  it left and the first.
- `lore-revision`: Fixed every command that realizes a move, such as `lore sync` and `lore branch switch`, a layer's
  mount included, `lore branch merge`, `lore revision cherry-pick`, `lore link update` and the reset or unstage of a
  staged link, leaving a moved file or directory beside its destination, two spellings on disk, where the working tree
  holds the move's source in another case than the revision does. The move renames that entry where it is the only
  entry of that name in the source's directory, and of its kind, a file only where it holds the source's content in
  the revision the working tree stands on or the content the move carries. A file holding other content stays in
  place. The warning a realize ends with for the renames it left undone reads "Left N renames undone" and names the
  first.
