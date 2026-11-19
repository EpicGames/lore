---
type: improvement
---

- `lore-revision`: A merge, cherry-pick, revert, branch or revision diff, restore, switch or sync
  that compares two revisions whose trees the local store lacks fetches the parts of the two trees
  that differ ahead of the comparison, rather than only as the comparison reaches them. In a bare
  instance or a fresh shared store, a merge of two branches that parted long ago finds its
  conflicts much sooner. A comparison limited by a view or to one path fetches as before.
