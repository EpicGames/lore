---
type: improvement
---

- `lore-server`: a `lore.revision.v1` `BranchList` call reads the branch names, metadata and latest
  revisions of a repository with one eventually consistent range read each, rather than two
  strongly consistent reads per branch, so it costs the mutable store far less and is throttled
  less often. A listing can be about one second out of date: a branch created or archived just
  before the call can be missing from it or still in it, and a latest revision can be the one
  before. A branch whose metadata the read misses is left out of that listing, and a branch whose
  latest revision it misses is listed with a zero latest. With `include_deleted`, a live branch
  whose name the read misses is listed as deleted. A branch that several names map to is sent
  once, where it was sent once per name.
