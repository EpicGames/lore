---
type: improvement
---

- `lore-io`: `lore repository instance clone` holds a bounded amount of memory as it clones or copies an instance's files, whatever their number or the width of a directory, and the threads it runs on share the work of a directory with many entries.
