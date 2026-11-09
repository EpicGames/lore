---
type: fix
---

- `lore-server`: Fixed a server over a `remote` immutable store answering a fragment the remote
  lacks as a store failure and logging a warning for it. The fragment is answered as not found.
