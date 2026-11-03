---
type: improvement
---

- `lore-io`: on Windows, reading a directory queries the file system with a larger buffer, so a wide directory takes
  fewer queries, each of which every filesystem filter driver, such as antivirus, also sees. Every directory the library
  reads is read this way: the working-tree scans of `lore status --scan` and `lore stage --scan`, `lore stage` and
  `lore file dirty` on a directory, `lore reset --purge`, `lore link add`, directory renames in the working tree, and the
  local immutable store's index, among others. A directory is read as before when its path is relative, or when its
  search pattern, the path with `\*` appended, is longer than `MAX_PATH` (260 UTF-16 units with the terminator) without
  the `\\?\` prefix, or 512 units or longer with it.
