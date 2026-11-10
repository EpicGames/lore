---
type: fix
---

- `lore-revision`: Fixed `lore reset --purge` deleting a committed file whose name the working tree
  spells in another case, such as `foo.txt` for a committed `Foo.txt`. On Windows and macOS that file
  is the committed file itself. The purge leaves it to the reset.
