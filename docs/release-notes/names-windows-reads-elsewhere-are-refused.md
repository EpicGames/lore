---
type: breaking
---

- `lore-revision`: Names Windows reads as something else are refused on every platform and skipped where a revision
  already holds one, so a clone or sync on Windows writes nothing outside the working tree: a drive (`C:`, `a:b`), a
  name ending in a dot, a space or a colon (`file.`, `...`), a stream type (`file::$DATA`), and the `.lore` directory's
  short name (`LORE~1`). Rename such a file or add it to `.loreignore`. Other names with a colon, such as `12:00.log`,
  are unaffected.
