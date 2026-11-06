---
type: fix
---

- `lore-revision`: Fixed SWFS errors, and SWFS callbacks that fail, such as a read whose content cannot be found, going
  unlogged. They are now logged as errors as well as reported by `lore service status`
