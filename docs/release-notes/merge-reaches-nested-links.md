---
type: fix
---

- `lore-revision`: Fixed `branch merge start` dropping the changes a merged branch made inside a
  link nested in another link, and removing the merged files from disk there. The merge of a
  linked repository now merges the links it holds as the main repository's merge does, and
  `branch merge resolve`, `branch merge conflicts list` and `branch merge abort` reach their
  conflicts, a link row both branches changed included.
- `lore-revision`: Fixed `branch merge start --link` leaving the conflicts of its merge
  unmarked in the working tree. It writes the conflict markers at the link's mount, as a merge
  of every link does.
