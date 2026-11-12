---
type: fix
---

- `lore-revision`: Fixed `lore unstage` of a link's staged removal leaving the link staged for removal and dirty, though its files were back in the mount, where the link sits below a directory or anything else stays staged or dirty.
