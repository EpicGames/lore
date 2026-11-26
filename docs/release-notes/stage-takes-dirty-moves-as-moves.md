---
type: fix
---

- `lore-revision`: Fixed `lore stage` leaving out a move marked with `lore file dirty move` whose file kept its content: naming its destination, a directory above it, or the whole tree, with or without `--scan`, answered "No changes staged", and the next commit left the move out. The stage now takes the move.
- `lore-revision`: Fixed `lore stage` of a file moved with `lore file dirty move` and then edited committing a modify at the new path, which lost the move from the revision's delta and the file's history. The commit now records a move that carries the edit, and `lore file history` continues at the old path.
- `lore-revision`: Fixed `lore stage` of the path a `lore file dirty move` left failing with "Invalid path". Naming that path, alone or beside the destination, now stages the move, and a new file marked dirty at that path is staged beside it.
- `lore-revision`: Fixed a directory moved with `lore file dirty move` that holds no file, empty or holding only directories, never being staged: `lore stage .` and `lore stage --scan .` now stage its move. Staging a moved directory stages the move of what moves with it, and leaves out a new file nothing marked.
- `lore-revision`: Fixed `lore status` reporting a directory moved with `lore file dirty move` as an add of its new path and nothing at its old one. It now reports the move, such as `V d/ -> e/`, with each directory below it, in the plain and `--scan` listings and in the status events.
