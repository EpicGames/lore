---
type: fix
---

- `lore-revision`: Fixed `lore stage`, `lore unstage` and `lore dirty` under several mounts of one
  linked repository giving each mount a revision of its own, and moving a mount the stage left
  unchanged. The mounts holding a change pin one revision, and `lore status --check-dirty` shows
  the dirty files below each. A commit of changes made in the working tree under two mounts of
  one repository is still refused.
- `lore-revision`: Fixed a failed commit leaving a linked repository's branch at the revision it
  committed, which the next `lore push` published although no revision of the parent pinned it.
  A commit that fails before it stores the parent moves each linked branch back, unless another
  commit moved it since.
- `lore-revision`: Fixed a commit or `lore link update` refused by a local change at one mount
  having already rewritten the files of another mount of the same repository.
