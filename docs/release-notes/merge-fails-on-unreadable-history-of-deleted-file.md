---
type: fix
---

- `lore-revision`: Fixed `lore branch merge`, `lore branch diff` and Lore Server's diffs resolving a
  conflict over a deleted file wrongly when a revision of the deleting branch's history could not be
  read. They now fail instead of counting the revision as one without the file.
