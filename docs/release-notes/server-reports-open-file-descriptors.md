---
type: feature
---

- `lore-server`: On Linux and macOS, the server exports `lore.server.open_file_descriptors`, the file descriptors
  it holds open, and `lore.server.file_descriptor_limit`, its soft `RLIMIT_NOFILE` limit, sampled at each metrics
  export. Every socket counts against the limit, and once the server reaches it, every connection or file it opens
  fails with "Too many open files". The limit gauge has no sample while the limit is unlimited.
