---
type: feature
---

- `lore-revision`: an instance reads its own ignore rules from `ignore` in its Lore directory (`.lore/ignore` for a
  regular instance), in the syntax of `.loreignore`, after the rules of `.loreignore`. Like `.loreignore`, they
  apply only to paths the tree doesn't track yet, and are never committed. They can only exclude: a `!` line is skipped
  with a warning naming it. A file that cannot be decoded fails the filter load with an error naming the file.
