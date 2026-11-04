---
type: fix
---

- `lore-revision`: Fixed staging a file over a link leaving the link's registry entry behind, so a
  clone listed a link at a path holding a file. Staging the file now removes the link.
- `lore-revision`: Fixed a folder staged where `lore link remove` had removed a link failing with
  `Link not found` and being left out of the commit. The removal and the folder or file replacing it
  now commit together.
- `lore-revision`: Fixed `lore reset` of a committed folder or link failing with `File exists` or
  `Not a directory` where a file stands at its path. Reset of a link a file replaced now puts the
  link back.
- `lore-revision`: Fixed `lore unstage` of a staged link add or removal deleting or overwriting work
  at the mount. It refuses where the work is a local change to a file the link holds or a file
  standing at the mount, and points at `lore reset`. Files the link does not hold stay at the mount.
- `lore-revision`: Fixed `lore unstage` of a path whose type change is staged unstaging only the
  added node, and panicking where the replaced node was a link that was never committed.
