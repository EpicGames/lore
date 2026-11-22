---
type: fix
---

- `lore-revision`: Fixed the first `lore status --scan` after `lore link add`, `lore layer add`, or a sync that brings in
  a new link, hashing every file under the new mount. These commands now record the modified time of each file they
  write, as a clone does, so the scan reads them as unchanged without hashing them.
