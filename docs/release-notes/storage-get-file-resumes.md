---
type: improvement
---

- `lore`: `lore_storage_get_file` and `lore_storage_get_file_resolved` resume a multi-fragment write
  of 32 MiB or more that failed or stopped: a later call writing the same content and range to the
  same path fetches only what the earlier one did not write, and leaves the content's holes
  unwritten, as a write that does not resume does. A resume reads back what was written, so on a
  slow disk it can take longer than fetching again over a fast link. A resume that finds data in a
  hole of the content, as it does in every hole holding a whole block on a file system that
  reports no holes, writes the whole range again.
  Shorter writes start over, as before. Writes of 32 MiB or more to one path wait for each other.
