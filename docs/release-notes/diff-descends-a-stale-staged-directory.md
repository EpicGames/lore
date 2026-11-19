---
type: fix
---

- `lore-revision`: Fixed `lore sync` and `lore branch switch` leaving a deleted file on disk when the instance was at a revision an earlier server stored for a fast-forward merge, and the file was deleted below a directory the merge changed. Such a revision holds that directory with the address it had before the merge, and a diff from it took the directory as unchanged wherever the target held that address. A diff from such a revision reports every change below that directory, so a sync, a branch switch, `lore revision diff` and a merge based on it report the delete. A merge whose view leaves the directory out keeps the current branch's delete below the directory rather than taking the other branch's whole directory over it.
