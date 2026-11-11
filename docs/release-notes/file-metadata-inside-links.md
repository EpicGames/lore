---
type: fix
---

- `lore-revision`: Fixed `lore file metadata` on a path inside a link changing or reading an unrelated parent file.
  `set` and `clear` now refuse such a path, and `get`, `list` and `lore file dump` read the linked file.
- `lore-revision`: Fixed `lore file metadata set` succeeding without setting anything on a missing path or past the
  1 MiB metadata limit.
