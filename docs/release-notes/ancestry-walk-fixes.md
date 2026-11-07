---
type: fix
---

- `lore-revision`: Fixed a push, sync or revision lookup, in its walk back to the newest revision
  the local and remote latests share, finding an older shared revision when the local line reaches
  the newest one before the remote line does, which could report a branch the remote is ahead of
  as divergent.
- `lore-revision`: Fixed that walk asking the remote again, on every step, for a line the remote
  had already ended.
- `lore-revision`: Fixed that walk reading a failed request for a line's revisions as the end of
  the line, which could report lines that share a revision as divergent. A walk that can get no
  further fails with the remote's error, such as `Disconnected` (28) or `NotAuthorized` (17).
- `lore-revision`: Fixed that walk asking the remote for revisions the local store holds once a
  remote page led it to one, which cost a round trip per page. It also no longer asks the remote
  in a step where the two lines already meet among the revisions it has read, or both reach the
  same revision the local store lacks.
- `lore-revision`: Fixed that walk looping without end on a remote whose pages return to a revision
  they already listed, or don't move the line at all. The first fails the walk with an internal
  error, as does a line whose revision numbers don't decrease, or a page the walk needs that lists
  a revision on the line without its own state. The second stops the line, so a walk that can get
  no further fails with `RevisionNotFound` (88) naming the revision it stopped at. A revision
  lookup whose walk fails answers the local latest.
- `lore-revision`: Fixed that walk taking time that grows with the product of the two lines'
  lengths, which slowed a push or sync of a branch far from the remote's latest. It now takes time
  that grows with the revisions it walks.
- `lore-revision`: Fixed a revision lookup by number trusting the number a remote page lists beside
  a revision, which could resolve it to a revision with another number. It takes the listed
  revision only where the revision's state carries the number, and otherwise searches the history.
- `lore-revision`: Fixed a three-way merge or diff taking a base older than the newest revision the
  two lines share when one line reaches that revision a batch before the other, which can report
  conflicts the newest base wouldn't. A line whose revision numbers don't decrease fails the search
  for that base with an internal error, as does a page that lists a revision on a line without its
  own state. Such a page also fails the search for the layer revision whose metadata matches a
  revision.
