---
type: breaking
---

- `lore-revision`: `.loreignore` applies only to paths the tree doesn't track yet. A committed file an ignore rule
  matches, such as one staged with `--force`, keeps flowing through like any other: `lore status --scan` and
  `lore status` report changes to it, `lore stage` and `lore dirty` pick them up without `--force`, and removing it,
  or the directory holding it, from the working tree stages its deletion. Previously only a change already staged was
  listed, and an unforced `lore stage` kept a removed file the ignore file names. New files the ignore file names are
  still left out, including a copy marked with `lore dirty copy` until it is staged, as is a directory that replaces
  a tracked file, or the reverse, where the rules exclude it. A file moved with `lore dirty move` stays tracked at its
  new path. See
  [ADR-00027](../developing/decisions/00027-ignore-filter-applies-to-untracked-paths-only.md)
