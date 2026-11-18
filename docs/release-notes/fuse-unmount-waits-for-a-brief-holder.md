---
type: fix
---

- `lore-fuse`: Fixed `lore repository instance move`, `lore repository instance delete` and
  `lore service stop` failing with `Mounts are busy` when a program held a FUSE virtual instance
  for a moment. They wait up to five seconds for the mount to be free before they report it busy.
