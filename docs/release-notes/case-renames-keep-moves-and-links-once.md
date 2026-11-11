---
type: fix
---

- `lore-revision`: Fixed `lore branch merge`, `lore push --fast-forward-merge`, `lore revision cherry-pick`,
  `lore revision revert` and `lore sync` reporting a conflict where one side renames a directory only in case and edits
  a file below it, and the other side moves that file. The merge now takes the rename, the move and the edit: the file
  stands where the move put it, with the edit, and below the directory's new spelling where the move keeps it in the
  directory.
- `lore-revision`: Fixed `lore branch merge`, `lore revision cherry-pick` and `lore revision revert` leaving a second
  spelling of a directory in the working tree where one side renames the directory only in case and edits a file, and
  the other side moves that file into the directory.
- `lore-revision`: Fixed `lore branch merge`, `lore push --fast-forward-merge`, `lore revision cherry-pick`,
  `lore revision revert` and `lore sync` where each side renames a directory only in case, one directory inside the
  other, and one side edits a file below both. Where the other side leaves the file, the edit stood below the inner
  directory's old spelling, and a merge into the working tree left both spellings of it. Where the other side moves the
  file, the merge reported a conflict. The edit now stands below both new spellings.
- `lore-revision`: Fixed the same commands reporting a conflict where both sides rename a directory or a file only in
  case to the same name, or move a file to the same place, and one side also renames a directory above it only in
  case. The two renames, or the two moves, are one.
- `lore-revision`: Fixed `lore branch merge`, `lore revision cherry-pick` and `lore revision revert` where both sides
  rename a directory only in case to the same name, and the merged side moves or deletes a file below it, or renames a
  directory below it only in case. The move left the file at its old path beside the moved copy, the delete left the
  file, and the rename left the directory as it was, with an empty one of the new name beside it. A move, a delete or a
  rename only in case of a file or a directory below such a rename now takes its node from where the working tree holds
  it.
- `lore-revision`: Fixed the same commands taking one of two files added at one place below directories renamed only
  in case, however the two sides split the renames, with no conflict. The two adds now conflict.
- `lore-revision`: Fixed the same commands reporting a conflict where one side renames a directory only in case,
  deletes a file below it and adds it again with the same content, and the other side edits or moves the file. The
  delete and the add are no change, as where no rename comes with them.
- `lore-revision`: Fixed `lore branch diff`, `lore status` and the server's diffs listing a link twice where it is
  renamed only in case, and either given another pin or mount where the diff doesn't descend into it or staged with
  nothing staged below it.
