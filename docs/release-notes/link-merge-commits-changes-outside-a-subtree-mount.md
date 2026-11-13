---
type: fix
---

- `lore-revision`: Fixed the commit of a merge across a link that mounts a subtree of the linked
  repository, which dropped what the merge changed outside that subtree, committed a conflict
  there unresolved, or failed with `Staged node remain after nodes were committed` where the merge
  also changed something inside the subtree. The commit, `commit --link` included, now commits the
  whole linked tree. It refuses a conflict left outside the subtree, naming it as
  `linked/repo: config/settings.txt (outside /content/assets)`, before anything is committed;
  `lore branch merge abort` undoes such a merge.
- `lore-revision`: `lore branch merge` now refuses, before it stages anything, a merge whose linked
  branch deletes the subtree a link mounts or a directory above it, naming the link and the
  subtree. Such a merge failed at its commit and had to be aborted.
