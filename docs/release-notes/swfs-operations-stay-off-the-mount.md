---
type: fix
---

- `lore-revision`: Fixed a command on an SWFS instance reading the working tree through the mount, which could hang the
  service, after a remount or a revision change, or for a path an ignore rule matches. Where it cannot tell what the
  user changed, the command now fails rather than risk the hang.
