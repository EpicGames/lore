---
type: improvement
---

- `lore-io`: on Windows, checking the type, size and modification time of a named path takes one query, which the file
  system answers without opening the file, where it took an open, a query and a close that every filesystem filter
  driver, such as antivirus, also saw. A filter driver that declines the query has the system open, query and close the
  file within that one call instead. Paths are checked this way for each dirty-marked or named file, as `lore status`,
  `lore stage`, `lore commit`, `lore reset`, `lore diff` and `lore sync` do; for each file `lore sync`,
  `lore branch switch`, `lore reset` and `lore branch merge` write or delete, and each move a rename can't make; for
  each file compared with stored content; for each directory a walk checks for a nested repository; and by `lore file write`,
  `lore file hash` and the local immutable and mutable stores. A symbolic link or other reparse point takes that query
  and then the open, query and close, except for a deletion, which checks the link itself with the query alone, and a
  path of `MAX_PATH` or more without the `\\?\` prefix is checked as before.
