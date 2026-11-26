---
type: fix
---

- `lore-revision`: Fixed `lore clone --direct-file-write` having no effect: the clone wrote its files through
  temporary files whatever the flag said. The flag now makes the clone write its files directly, for that clone only.
  The new instance's `[file] direct_write` setting is left as it is, and later commands such as `lore sync` follow it.
