---
type: fix
---

- `lore-server`: Fixed a revision tree request, through `ThinClientService.RevisionTree` or the
  earlier `RevisionTreeRequest`, leaving out a linked subtree, or reporting a tracking link as
  pinned, when the store refused a read as overloaded. The request now fails with
  `RESOURCE_EXHAUSTED`.
