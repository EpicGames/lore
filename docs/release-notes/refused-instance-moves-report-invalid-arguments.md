---
type: breaking
---

- `lore-revision`: `lore repository instance move` and `lore_repository_instance_move` refuse a target that already
  exists, is inside the instance (through a symbolic link too), is below a file, has no existing directory above it, or
  is already an instance mount with `InvalidArguments` (3), as a dry run does. A target that already exists or is
  already an instance mount was refused as an internal error (-1, exit status 255). Scripts and C API callers that test
  a refused move for 255 or -1 should test for 3. A symbolic link at the target that leads nowhere counts as existing,
  where the move replaced it
