---
type: breaking
---

- `lore-revision`: A link made by hand at `external/` in the global data directory, with no `external-root` file there, reads as an external root that can't be used: service start skips the virtual instances in it and reports why. Replace the link with the directory it names.
- `lore-revision`: `lore clone`, `lore repository create`, `lore repository instance clone`, `lore repository update-path` and `lore repository instance move` refuse with exit code 3 at a path where a virtual instance is registered while its external directory is there or can't be checked, as while its mount point is empty with the service stopped. Remove the virtual instance first, or use another path. A command that opens an unregistered instance for writing at such a path doesn't register it.
