---
type: fix
---

- `lore-server`: Fixed `ThinClientService.RevisionDiff` returning no changes when `from` is an ancestor of `to`
  only through a merge's second parent, such as after a branch is merged back. When one revision is an ancestor of
  the other, the diff is now 2-way with no `signatureBase`, and lists every change from `from` to `to`. Previously,
  when `to` was the ancestor, the changes were reported in the wrong direction.
