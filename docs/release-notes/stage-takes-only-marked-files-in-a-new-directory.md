---
type: fix
---

- `lore-revision`: Fixed `lore stage` and the replay of dirty marks after a sync, a branch switch or a commit taking
  in every file of a new directory where only some were marked, such as after `lore status --scan dir/a.txt` with
  `dir/b.txt` beside it. A new directory that is only marked is staged along with the files marked in it, and on its
  own only where it is empty. A directory `lore dirty --force` creates above a new file is untracked, as the file is,
  so `lore status` no longer lists it below a directory the ignore file names
