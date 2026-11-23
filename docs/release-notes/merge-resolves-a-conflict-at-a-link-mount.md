---
type: fix
---

- `lore-revision`: Fixed `lore branch merge resolve mine|theirs` and a plain `resolve` refusing a
  conflict at a link mount, where one branch holds a link and the other a folder, a file, a link
  to another repository or nothing, with "Operation not supported". They now settle it. `theirs`
  replaces what the working tree holds at the mount with the merged branch's side, the link with
  its row, and refuses while the mount holds uncommitted changes. A following link's repository
  is merged into the linked branch the link lands on, once for all its mounts, as for a link a
  merge brings in. A mount conflict resolved to one side can be resolved to the other, except
  back to this branch's link once the merge merged its repository.
- `lore-revision`: Fixed a merge dropping a link that one branch removed and the other changed,
  through its mount or in its row. It conflicts at the mount, which resolves as above, with `mine`
  keeping both branches' changes to the linked repository.
- `lore-revision`: Fixed `lore branch merge abort` leaving out what the branch held at a link
  mount after a conflict there was resolved to the merged side. Abort puts it back.
