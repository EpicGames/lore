---
type: fix
---

- `lore-revision`: Fixed `lore repository instance move` and `lore_repository_instance_move` moving an instance into
  another repository's working tree, or into a virtual instance's mount. Such a target is refused with code 3
  (invalid arguments), as a target inside the instance itself is. Creating or cloning an SWFS instance inside a FUSE
  instance's mount is refused the same way.
