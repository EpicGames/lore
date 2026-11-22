---
type: fix
---

- `lore-revision`: Fixed the history walks of `lore push`, `lore sync`, `lore branch merge`,
  `lore branch diff` and layers taking a revision that a faulty server lists in a history page as
  part of a branch's history when it isn't. Each revision in a page must name the next as its
  first parent. A walk that reads a page that doesn't, or whose cursor doesn't name the first
  parent of its last revision, fails with an internal error. The search for a base revision keeps
  only the part of a page that follows the line.
