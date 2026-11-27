---
type: fix
---

- `lore-server`: Fixed a repository create, delete or lookup overwriting or removing the name of
  another repository that took the name at the same time. That repository's lookups by name and
  by ID then answered not found. A create that loses the name to another repository now answers
  `ALREADY_EXISTS`, and the other writes leave a name held by another repository as it is.
