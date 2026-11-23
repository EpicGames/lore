---
type: fix
---

- `lore-server`: Fixed the legacy `RepositoryQuery` answering a repository the caller may not look
  up with a different `NOT_FOUND` message than a missing one, which told the caller that the name
  exists.
- `lore-server`: Fixed `RepositoryDelete` answering `PERMISSION_DENIED` for a repository that none
  of the caller's grants reach, which told the caller that the repository exists. It now answers
  `NOT_FOUND`, as for a missing repository.
