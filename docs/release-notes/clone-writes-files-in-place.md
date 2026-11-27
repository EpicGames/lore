---
type: improvement
---

- `lore-revision`: `lore clone`, and `lore_repository_clone` in the C API, write every file at its own path, with no
  temporary file, on every platform and whether or not `--direct-file-write` or `[file] direct_write` is set. A file
  of more than one fragment no longer costs a temporary file and a rename. A clone that fails or stops part way can
  leave a file partly written at its own path. On Windows `--direct-file-write` still writes each file of one fragment
  past the file cache.
