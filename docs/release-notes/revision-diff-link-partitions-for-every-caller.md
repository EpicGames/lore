---
type: fix
---

- `lore-server`: Fixed `ThinClientService.RevisionDiff` announcing no `DiffPartition` in a 3-way diff where one side
  re-pins a link to a revision with the same mounted content, for a caller who may read the linked repository. A
  caller who may not got one. Any link the diff adds, removes, re-pins or remounts now announces its partition to
  every caller, so a stream with no `DiffPartition` is the same for every caller.
