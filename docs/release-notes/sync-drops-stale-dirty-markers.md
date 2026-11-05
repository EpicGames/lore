---
type: fix
---

- `lore-revision`: Fixed `lore status` reporting files as modified after `lore sync` or `lore branch switch` restored
  them, when `lore status --scan` had marked them as changed before. Local changes stay marked.
- `lore-revision`: Fixed `lore sync` marking a locally edited layer file as deleted, or a restored one as modified,
  including in a layer whose source path differs in case from the path its repository holds.
- `lore-revision`: Fixed `lore status` hashing every file a sync or switch wrote into a layer, by recording their
  modified times as it does for the files of the repository itself.
