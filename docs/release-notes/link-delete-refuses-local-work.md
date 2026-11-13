---
type: fix
---

- `lore-revision`: Fixed `lore sync`, `lore branch switch` and `lore branch merge` silently deleting a local edit in a
  link's mount when the revision drops the link or moves it to another repository. They refuse with
  `LocalModifications`, naming the edited files. `--force` discards them, and `sync --forward-changes` keeps them.
- `lore-revision`: Fixed removing a link's mount deleting what the link doesn't track. A sync, a switch, a merge and
  `lore link remove` remove the files the link tracks and the folders they leave empty, as a deleted folder is removed,
  and keep an untracked file, a symbolic link, a nested repository and what the ignore or view rules exclude, reporting
  what they kept. They refuse where a kept entry stands where the new revision writes.
- `lore-revision`: Fixed a mount replaced by a symbolic link being followed when its link is removed, which could delete
  files outside the repository. The symbolic link is kept and nothing below it is removed.
- `lore-revision`: Fixed a link mount holding a directory that can't be listed, or a file that can't be read, being
  removed though it may hold local work. The removal is refused unless `--force` is given.
- `lore-revision`: Fixed `lore link remove` checking and deleting the mount under the path as typed rather than as the
  tree spells it, which on a case-sensitive disk is a different directory.
