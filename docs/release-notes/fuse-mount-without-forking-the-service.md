---
type: improvement
---

- `lore-fuse`: Mounting a FUSE virtual clone no longer stalls the Lore service's other mounts
  while it starts `fusermount3`: the service no longer copies its own memory to start it.
