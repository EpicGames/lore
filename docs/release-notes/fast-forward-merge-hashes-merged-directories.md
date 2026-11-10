---
type: fix
---

- `lore-server`: Fixed `lore push --fast-forward-merge` storing a merged revision whose directory
  hashes did not match the files below them. An instance that synced from such a revision to an
  earlier one kept the merged changes in its working tree and reported none, and a revision
  committed on top of one recorded the merged directories as changes of its own. Revisions an
  earlier server merged keep the directory hashes they were stored with.
