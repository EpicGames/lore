---
type: fix
---

- `lore-revision`: Fixed `lore branch merge start` failing with `Not a directory` where one branch replaced a file with
  a folder and the other edited it. It now reports a conflict at that path, which resolves to either side.
- `lore-revision`: Fixed `lore branch switch` and `lore sync` failing with `Not a directory` where they replace a file
  with a folder, or a folder with a file.
- `lore-revision`: Fixed `lore branch merge start` failing where both branches added the same path, one as a file and
  the other as a folder. It now reports a conflict at that path, which resolves to either side.
