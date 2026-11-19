---
type: improvement
---

- `lore-revision`: `lore branch merge` and `lore branch diff` read long stretches of the branches'
  history sooner, both to find a base revision far back in it and to find where a file that one
  branch deleted was last held. Lore requests several pages of that history from the remote at
  once, ahead of where it reads, rather than one page after another.
