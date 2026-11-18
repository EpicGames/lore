---
type: fix
---

- `lore-revision`: Fixed `lore unstage` of a path below a directory staged for deletion leaving the path's deletion to the commit, which deleted the whole directory and recorded the unstaged path as kept. The unstaged path now stays as committed, with the directories of the deletion above it, and the commit deletes only the rest of the staged deletion, in a link's repository and a layer as in the repository's own tree.
- `lore-revision`: Fixed a commit deleting a directory the working tree no longer holds with everything in it, files never staged included, where only a change below the directory was staged, such as an add or a move a merge brings into it. The commit now deletes only what was staged: it keeps the directory with what in it was not staged, which `lore status --scan` shows as deleted and not staged, and each directory above it that the working tree no longer holds and that is staged only for a change below it. Below a directory the working tree no longer holds that is staged with a change of its own, such as a move, what lies below goes with that change.
- `lore-revision`: Fixed `lore commit` taking into the revision a file marked with `lore file dirty copy` and not staged. The copy now stays out of the commit, as a file marked dirty as added does, and stays marked dirty past it, as added.
