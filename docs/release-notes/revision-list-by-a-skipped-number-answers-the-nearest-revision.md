---
type: fix
---

- `lore-server`: Fixed a revision list by a revision number that a branch's history line skips,
  as after merging in a busier branch, answering not found after walking the branch's history
  from its latest revision. It now answers the page holding the line's nearest revision numbered
  below that number, and a number with no revision of the line at or below it is not found. The
  walks the lookup makes record where the line skips numbers, so later lookups in that range read
  the record instead of walking.
- `lore-server`: Fixed a revision list by revision number on a branch that was force-pushed back
  answering revisions of the history line it left, after a lookup on a branch made from that line.
  A lookup records where a line skips numbers under the branch whose line it walks.
- `lore-server`: Fixed a revision list by revision number answering not found when the server
  can't read the branch's latest revision. It answers an internal error.
