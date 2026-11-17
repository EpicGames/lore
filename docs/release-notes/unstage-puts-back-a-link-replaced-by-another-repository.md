---
type: fix
---

- `lore-revision`: Fixed `lore unstage` and `lore reset` of a link removed and added again to
  another repository: they refused with "Operation not supported", or, with a change staged inside
  the link, kept the new link. They now put the original link back, in the link list and on disk,
  as they do where the new link was removed again. `lore unstage` refuses where the new link holds
  local changes; `lore reset` discards them.
