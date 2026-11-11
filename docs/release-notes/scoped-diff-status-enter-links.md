---
type: fix
---

- `lore-revision`: Fixed `lore file diff <path>` at a link's mount path reporting every file under the mount as added,
  and `lore status --scan <path>` there reporting the mount as deleted with its files untracked. A diff or status of a
  path at or below a mount reports what the same command over the whole repository reports under that path, including
  for a link not yet committed, a mount missing from the working tree or holding a file, and links inside the linked
  repository.
- `lore-revision`: Fixed `lore status --scan <path>` for a new folder below a link mount changing the link in the staged
  tree, and for a new file there failing with a not-found error. Both report the new files as the same command over the
  whole repository does.
- `lore-revision`: Fixed `lore file diff <path>` and `lore status --scan <path>` below a link mount whose linked
  revision can't be read reporting every file under the path as added. They fail as the same command over the whole
  repository does.
- `lore-revision`: Fixed `lore file diff <path>` and `lore status --scan <path>` for a path the repository holds as a
  file and the working tree as a folder leaving out the file's delete. They report the file deleted and the folder
  added, as the same command over the whole repository does.
