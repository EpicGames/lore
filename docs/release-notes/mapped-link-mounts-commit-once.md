---
type: breaking
---

- `lore-revision`: Fixed `lore commit` refusing changes made in the working tree under two mounts
  of one linked repository. The commit makes one revision of the repository holding the changes
  under every mount, and every mount pins it. `lore commit --link <mount>` commits the changes
  under every mount of the repository too, and moves every mount, where it moved only the one
  named and refused a repository mounted more than once.
- `lore-revision`: Fixed a commit inside a link publishing over a revision that another commit
  stored on the linked branch since the parent pinned it. Such a commit fails with
  `BranchAdvanced`, naming the link; `lore branch merge start <branch> --link <mount>` merges the
  newer revisions into it.
- `lore-revision`: `lore link add`, and a reset restoring a removed link, refuse to mount a
  folder of a repository that another mount already exposes, as a commit could take the changes
  of only one of the copies. Mount another folder instead. A commit through two mounts that
  expose one folder, added before this release, fails with `InvalidArguments`; remove one of them
  with `lore link remove`.
