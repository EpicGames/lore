---
type: fix
---

- `lore-revision`: Fixed a command that reads a revision reporting `Oversized` (118) on a transient store failure, rather than the store's own error, such as `AddressNotFound` (80) or `Disconnected`.
