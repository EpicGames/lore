---
type: fix
---

- `lore-server`: Fixed the revision a fast-forward merge (`lore push --fast-forward-merge`) stores differing from the one `lore branch merge` of the pushed revision commits. A moved file kept neither its identity nor its history, `lore file history` named the wrong revision for the files either side changed, `lore revision info --delta` listed the changes of the branch's previous latest, and a file's metadata or a change both sides made alike was left out of the merge. The merge revision now records the same tree, delta and file history as a merge a client commits.
- `lore-revision`: Fixed `lore branch merge` committing the old content of a file outside the view that the merged branch moved and edited.
