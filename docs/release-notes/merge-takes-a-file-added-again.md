---
type: fix
---

- `lore-revision`: Fixed `lore branch merge`, `lore sync`, `lore revision cherry-pick` and a push with `--fast-forward-merge` keeping a file's old file ID where the other branch deleted the file and added it again with the same content. The branch merged into kept the old file, so the two branches differed by file ID alone, and `lore revision diff` and change requests listed the file as modified. They now take the added file with its own file ID, and `lore branch diff` lists it. Where the branch merged into changed the file too, its change stands, with no conflict. A file ID a branch holds only because an earlier merge kept it is no change of that branch: a merge of the branch into the other keeps the other's file, and `lore branch diff` doesn't list it.
