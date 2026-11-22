---
type: improvement
---

- `lore-revision`: A push, sync or revision lookup, in its walk back to the newest revision the
  local latest and the remote latest share, asks the remote for a line's revisions only once the
  other line can't meet it among the revisions the local store holds. Where the lines meet in the
  store a step after one line reaches a revision the store lacks, the walk makes no request to the
  remote, which saves a round trip.
