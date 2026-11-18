---
type: improvement
---

- `lore-revision`: `lore branch merge`, `lore branch diff` and Lore Server's revision diff no longer
  search history for whether a conflicting change was already merged, where no merge joins the two
  branches since they parted. Such a conflict stands without the search, so a large merge no longer
  appears to hang before it reaches its conflicts.
- `lore-revision`: Where the search runs, as in a merge of branches a merge already joins and in
  Lore Server's merge of a push, each branch's history is walked once for all the files it deleted,
  instead of twice for each file. A file deleted inside a link is not looked for in the linked
  repository's history.
- `lore-revision`: The search for the base of a branch diff or merge reads each branch's history
  from the remote a page of revisions at a time, instead of one revision per request.
