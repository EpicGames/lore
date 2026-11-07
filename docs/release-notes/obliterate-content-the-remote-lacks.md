---
type: fix
---

- `lore-revision`: Fixed `lore file obliterate` failing with `NotFound` for content the remote does
  not hold, such as content never pushed, after it had removed the local copy. The command now
  obliterates on the remote first and counts content the remote does not hold as obliterated
  there, so it removes the local copy and succeeds. A failed remote obliterate leaves the local
  copy in place.
