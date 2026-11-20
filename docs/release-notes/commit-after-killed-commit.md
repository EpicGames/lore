---
type: fix
---

- `lore-revision`: Fixed `lore commit` failing with `Branch has been advanced by another instance`
  or `Staged state has stale parent` after an earlier commit was killed while it wrote its result,
  though no other instance had committed. The next `lore commit` or `lore revision amend` finishes
  the killed commit first: its revision becomes current. Where something was staged after the kill,
  the command still fails, with `The branch's latest revision has moved on`, leaves everything as
  it is and warns why: unstage what was staged, or run `lore sync`, then stage it again. With
  `--force`, the command runs over the state the kill left, as before.
- `lore-revision`: Fixed a killed `lore commit` or `lore revision amend` leaving nothing for the
  next command to finish it with: each now writes its revision to disk before it returns, rather
  than after, so it takes longer to return where data sync is on.
