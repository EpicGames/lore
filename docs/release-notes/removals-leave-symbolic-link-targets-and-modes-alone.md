---
type: fix
---

- `lore-revision`: Fixed a removal that failed on Unix making what it named, or the directory a symbolic link there
  stood for, writable by everyone, while still failing. A removal takes a symbolic link as the link and does not follow
  it: `lore sync`, `lore branch switch`, `lore reset` and `lore branch merge` remove a symbolic link standing where a
  tracked directory was without changing the mode of the directory it stands for, though a file the revision tracks
  below the link is still removed through it, and they remove a symbolic link that names nothing instead of leaving it
  in place. On Unix, a failed removal of a file or an empty directory is not retried, since its own mode never refuses
  it, and a failed recursive removal is retried after adding only the owner's write permission to the directory itself,
  never through a symbolic link. On Windows, a failed removal is retried after clearing the read-only attribute of the
  name itself, a symbolic link's or junction's own rather than that of what it stands for.
