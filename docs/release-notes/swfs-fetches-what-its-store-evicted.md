---
type: fix
---

- `lore-revision`: Fixed an SWFS instance failing to finish a command with "Not found", leaving
  an empty working tree, and serving files as zeros, once its store had evicted the revision or
  the content it serves. The mount fetches what its store lacks from the remote and keeps it in
  the store, whatever a command was told about going offline. It connects the first time it needs
  to, with the credentials of the user who mounted it, or the service's own for a mount the service
  restores as it starts, and a mount whose connect failed, or whose connection was lost, connects
  again at the next command.
  Where the remote can't be reached, the read still fails, and `lore service status` lists the
  error.
- `lore-revision`: Fixed a FUSE mount whose connection to the remote was lost, after the transport
  gave up reconnecting it, failing every read of content its store lacks until the service
  restarted. The next command on the instance gives the mount its connection.
