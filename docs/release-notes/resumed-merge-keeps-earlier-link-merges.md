---
type: fix
---

- `lore-revision`: Fixed `lore branch merge start` dropping the merge of a linked repository when
  an earlier attempt failed at a later link and the merge was started again. The resumed merge
  merges that linked repository again, stopping on its conflicts, and keeps the conflicts resolved
  before the failure.
