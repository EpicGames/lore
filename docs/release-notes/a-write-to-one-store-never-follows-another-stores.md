---
type: fix
---

- `lore-storage`: Fixed a commit failing with "leader upload failed", and a storage put or upload
  (`lore_storage_put`, `lore_storage_upload` and their variants) completing without the content in
  its own local store, when another local store in the same process was writing the same content
  to the same repository or partition at the same moment, such as two instances of one repository
  in one service, each with a store of its own, or two in-memory storage handles.
