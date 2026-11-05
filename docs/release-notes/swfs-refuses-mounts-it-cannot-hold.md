---
type: fix
---

- SWFS: a service running commands for a caller with another global directory finds that caller's SWFS instances,
  where it reported `Repository not found`
- SWFS: `lore repository create` and `lore repository clone` refuse an SWFS instance once the service mounts eight,
  the most the SWFS driver attaches, with `Operation not supported: mounting another SWFS instance`. A ninth mount
  failed inside the SWFS library and left the client hung or disconnected. The count is the service's own mounts:
  the driver's limit is machine-wide
- SWFS: `lore repository create` and `lore repository clone` refuse a repository inside an SWFS instance's mount, and
  an SWFS instance inside another repository's working tree. Inside a mount they failed with `invalid path`
