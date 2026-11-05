---
type: feature
---

- `lore-revision`: an instance reads its own ignore rules from `ignore` in its Lore directory (`.lore/ignore` for a
  regular instance), in the syntax of `.loreignore`, after the rules of `.loreignore`. They apply wherever
  `.loreignore` does, tracked files included, and are never committed. They can only exclude: a `!` line is skipped
  with a warning naming it. A file that cannot be decoded fails the filter load with an error naming the file.
