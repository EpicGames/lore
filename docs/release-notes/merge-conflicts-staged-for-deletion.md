---
type: fix
---

- `lore-revision`: Fixed `lore branch merge`, `lore sync`, `lore revision cherry-pick` and `lore revision revert` keeping
  a file the incoming side deleted, where a view excludes the file and the current branch changed it. They take the
  incoming deletion.
- `lore-revision`: Fixed `lore commit` committing a file in conflict that was deleted and staged for deletion, alone or
  with its directory, before the conflict was resolved. The commit refuses it as an unresolved conflict.
- `lore-revision`: Fixed `lore commit` failing with `file not found` after `lore branch merge resolve`,
  `lore revision cherry-pick resolve` or `lore revision revert resolve` of a conflict whose file the working tree lacks,
  such as a file the current branch deleted and the incoming branch changed. `lore status` lists a file the current
  branch deleted as a deletion in conflict rather than an add, and `lore commit` refuses it until it's resolved. A plain
  resolve commits the file the working tree holds at the path, or its deletion where it holds none, and no copy the
  merge wrote beside the file is left once the merge commits. Outside a linked repository, no empty directory is left
  where the current branch deleted the file's directory; one that holds other files stays.
- `lore-revision`: Fixed `lore revision info --delta` of a merge revision listing a deletion, at the path or at another
  path, where the current branch deleted a file or a directory, the incoming branch changed a file there, and the merge
  kept the deletion. The merge revision records no change there. Fixed it too for a `lore sync` of diverged history,
  which listed a deletion the remote made, read at a file the merge holds, and could name a deletion the merge made
  after another file.
- `lore-revision`: Fixed `lore revision info --delta` of a revision `lore commit --force` committed from changes staged
  on an earlier revision, or `lore commit --link` committed through one mount of a linked repository after `--link`
  commits through another mount moved its branch on, listing the deletion of a file the revision before it added, and
  such a commit giving a file it kept the history of that add.
