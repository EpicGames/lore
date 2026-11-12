---
type: fix
---

- `lore-revision`: Fixed `lore branch switch` and `lore sync <revision>` from a revision holding a link to one holding a
  folder in its place, whichever replaced the other, deleting each file below the link that already held the folder's
  content, which left the folder empty with `status` reporting clean. That includes a local edit and a file a `--view`
  change brings into view. Such a file is kept in place rather than written again.
- `lore-revision`: Fixed a false `File has local changes` refusing a switch or sync from a link to a folder in its place
  where the folder's content differed from the link's. A sync restricted to a dependency set that reaches below the link
  removes it and writes the whole folder.
- `lore-revision`: Fixed a false `File has local changes` refusing `lore branch merge` of a branch where a folder
  replaced a link into a branch still holding the link, where the folder's content differed from the link's.
- `lore-revision`: Fixed `lore sync --forward-changes` from a link to a folder in its place deleting a local edit below
  the link that the sync forwards.
