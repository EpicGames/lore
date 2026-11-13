---
type: fix
---

- `lore`: Fixed `lore_shutdown` discarding, unflushed, what a revision tree handle still open wrote after its storage
  handle was closed. Shutdown flushes the stores of each such tree, as it does those of a storage handle still open,
  and logs a flush that fails.
