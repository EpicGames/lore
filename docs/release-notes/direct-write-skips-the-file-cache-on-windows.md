---
type: improvement
---

- `lore-storage`: On Windows, direct file write (`[file] direct_write`, or `lore clone --direct-file-write`) also
  writes each file of one fragment, at most 256 KiB, past the file cache, unbuffered, in one write, which leaves the
  lazy writer nothing to write back afterwards. Larger files, a file `lore_storage_get_file` writes sparse, and a file
  on a volume that refuses unbuffered writes are written through the file cache as before. The cost is on the first
  read: a file written this way isn't in the file cache after the command, so its first read comes from the device.
  On Linux and macOS direct file write is unchanged.
