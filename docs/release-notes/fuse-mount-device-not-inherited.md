---
type: fix
---

- `lore-fuse`: Fixed file operations in a FUSE virtual clone hanging, instead of failing, after the
  Lore service stopped serving it, while a process the service started during the mount was still
  running.
