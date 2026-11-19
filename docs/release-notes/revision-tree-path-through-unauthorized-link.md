---
type: fix
---

- `lore-server`: Fixed a revision tree request whose path crossed a linked repository the caller
  can't read, then a further link into one it can, returning the deeper repository's files instead
  of not found.
