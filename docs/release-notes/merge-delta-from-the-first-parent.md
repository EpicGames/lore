---
type: fix
---

- `lore-revision`: Fixed the changes recorded for a revision whose tree was started from another revision than its
  first parent: the merge revision of a `lore sync` of divergent history, `lore commit --force` of changes staged on
  an earlier revision, `lore commit --link` onto a link branch another mount advanced, and the merge revision of a
  link's divergent history. `lore revision info --delta` reported the changes staged against the revision the tree
  was started from, such as the remote's changes for a sync, and left out the local changes the revision makes from
  its first parent, a link whose pin it changes included. It now reports the change from the first parent, a move
  with its source there. While such a sync waits on a conflict, the commands that read the staged changes,
  `lore stage`, `lore status --scan`, `lore lock acquire`, `lore file info` and `lore unstage` among them, treat a
  local file an ignore rule matches as tracked. A revision committed on such a sync records the sync as the last
  change of the local files, where it recorded the sync as the last change of the remote's files.
- `lore-revision`: Fixed `lore file history` of a file merging brings in, which listed the merge revision as the
  file's add: it lists the file's history on the merged line, back to the revision that added it. A file
  `lore branch merge` brings in shows its history on the merged branch, where it started at the merge revision.
