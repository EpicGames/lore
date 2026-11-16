---
type: breaking
---

- `lore`: A multi-fragment `lore_storage_get_file` or `lore_storage_get_file_resolved` of a range
  of at least 32 MiB that fails, or that a process stops, keeps its temporary file
  `<path>.loretmp`, which can take disk space up to the size of the range, and writes
  `<path>.loretmp.rec` beside it, rather than removing the temporary file. A later call of at least
  32 MiB to the same path removes both once it completes, and one writing other content or another
  range there empties the temporary file first. A shorter multi-fragment call to the path writes
  the temporary file and renames it into place, leaving `<path>.loretmp.rec`; a single-fragment
  call, and one of the zero hash, leave both. To reclaim the space of a get you will not retry,
  delete both files.
