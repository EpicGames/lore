---
type: fix
---

- `lore-revision`: Fixed `lore branch merge` under a view filter, and a `lore sync` that merges a
  diverged branch under one, corrupting the merge where the merged branch pinned a link to another
  revision inside a subtree the view leaves out. Files of the merging repository took the content
  of files in the linked repository, and pushing the merge failed with `Address not found`. The
  link's pin now merges as in a merge with no view.
- `lore-revision`: Fixed `lore branch merge` under a view filter, and a `lore sync` that merges a
  diverged branch under one, losing a folder, file or link the merged branch put in place of one of
  another kind inside a subtree the view leaves out: a folder in place of a file or a link, a file
  in place of a folder, or a link in place of a folder. The merge now holds the path as the merged
  branch does.
