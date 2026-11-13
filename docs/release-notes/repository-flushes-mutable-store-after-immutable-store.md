---
type: fix
---

- `lore-revision`: Fixed a failed flush of the local store leaving a branch's latest revision, or an instance's current
  revision, naming data the local store couldn't find, and modified-time records that could hide changes from
  `lore status`. A failed flush after a command, or when creating or cloning a repository, is logged as a warning.
- `lore-server`: Fixed a failed flush at shutdown leaving branches naming data the server's local store couldn't find.
  The failure is logged as a warning.
