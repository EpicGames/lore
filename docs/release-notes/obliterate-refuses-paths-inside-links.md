---
type: fix
---

- `lore-revision`: Fixed `lore file obliterate` on a path inside a link destroying the content of a file in the parent
  repository, locally and on the remote, removing the path from the working tree, and staging that file's deletion. It
  now fails with `NotSupported` and changes nothing.
