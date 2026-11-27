---
type: fix
---

- `lore-revision`: Fixed `lore clone` (C API: `lore_repository_clone`) of an SWFS virtual instance whose file operation
  fails to start, such as when the mount can't be frozen, returning its error before the new instance recorded the
  branch it was cloned from: its name, its metadata and its latest revision. `lore clone` could exit with the instance
  left without them, and a C API caller could go on while they were still being recorded. The clone now records the
  branch before it returns the error.
