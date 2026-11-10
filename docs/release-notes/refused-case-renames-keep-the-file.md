---
type: fix
---

- `lore-revision`: Fixed a file or a directory being deleted where the file system refused to
  rename it to another spelling of its name, as a sync, branch switch, merge, `lore reset` or
  `lore stage --case` does. On Windows and macOS the new spelling names the file or directory
  itself, which the recovery from the refused rename removed, with every file in a directory. The
  file or directory now stays under its old spelling. `lore reset` and `lore stage` report the
  refusal, and a sync, branch switch or merge warns once with how many moves it carried out without
  the refused rename, and the first of them.
