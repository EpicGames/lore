---
type: fix
---

- `lore-revision`: Fixed `lore branch merge` and `lore branch diff` requesting pages of a branch's
  history from the remote again after receiving them. It happened where a page requested ahead
  listed only part of the history it covers, as where the remote failed the request for the rest,
  while searching for a base revision or for where a file that one branch deleted was last held.
  The fix applies to remotes whose history pages are at least as large as the pages Lore requests
  ahead.
