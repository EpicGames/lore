---
type: improvement
---

- `lore-storage`: On Windows, direct file write (`[file] direct_write`, or `lore clone --direct-file-write`) also
  writes files larger than one fragment past the file cache as their fragments arrive, which leaves the lazy
  writer nothing to write back afterwards. Measured on a Windows cloud VM, a whole clone of a test view of 2.8
  million files and 505 GiB took 15% less time, and left the machine's memory free where the file cache had used
  it all. A file `lore_storage_get_file` writes sparse, and a file on a volume that refuses unbuffered writes, are
  written through the file cache as before. On Linux and macOS direct file write is unchanged.
