---
type: fix
---

- `lore-revision`: Fixed a push, sync or revision lookup, in its walk back to the newest revision
  the local and remote latests share, reading a stored blob that isn't a revision state as a
  revision's number and parent where a latest or a parent names it, which could answer a wrong
  branch point. The walk fails with an internal error, whose trace names the revision, and a
  revision lookup whose walk fails answers the local latest.
- `lore-revision`: Fixed the search for the base of a three-way merge or diff taking such a blob
  for a revision on the line. The search stops reading the local store there, as it does at a
  revision the store lacks.
- `lore-revision`: Fixed the search for the layer revision whose metadata matches a revision. It
  never returned where the layer's line left the local store, at a revision the store lacks or at
  such a blob, and no remote listed it from there. It now compares the layer revisions the store
  holds, and fails with an internal error only where none matches. With `--search-nearest`, where
  the line of the revision being matched leaves the store in the same way, it compares that line's
  revisions the store holds, rather than only the revision itself. Without `--search-nearest`, a
  search that finds no match also fails where the layer's line ends, instead of after the search
  limit.
- `lore-revision`: Fixed a three-way merge or diff taking an older revision as its base, or failing
  as divergent, where a history line it follows runs past the first hundred revisions and leaves
  the local store further down, and no remote lists the line from there. The search for the base
  follows the line as far as the store holds it. Where the remote serves a line's revisions but
  fails to list them, and the store holds the revision the line starts from, the search no longer
  reads them one at a time past where the line leaves the store.
