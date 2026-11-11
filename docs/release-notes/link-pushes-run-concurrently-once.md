---
type: improvement
---

- `lore-revision`: `lore push` pushes the branches of the repository, its layers and the repositories they link up to 8
  at a time, and each linked branch once, at the newest revision linked, rather than once for every revision that links
  it. A branch is pushed once the branches it links have been, and the fragment queries and uploads of every branch
  share one bound.
- `lore-revision`: a link push that fails no longer stops the pushes of links it doesn't hold back. The push fails with
  the first failure in the order the links were reached, and reports each other one on a line of its own. A link that
  can't be planned, such as one whose revisions are on diverged lines, fails the push before anything is pushed.
- `lore-revision`: a push whose repository or layer has diverged from the remote fails before it pushes anything.
