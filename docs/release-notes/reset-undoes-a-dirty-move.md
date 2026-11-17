---
type: fix
---

- `lore-revision`: Fixed `lore reset` leaving a move marked by `lore file dirty move` in place, so `lore status` still showed it. A reset of the move's source, its destination, a directory above either or a path inside a moved directory moves the file or directory back to its committed path and clears the move, a rename that only changed the case of a name included, and so does a reset to another revision or to the last merge from a branch that does not hold the destination. A move stays where something else stands at its committed path. A reset of the destination alone deleted the moved file and left its committed path empty.
- `lore-revision`: Fixed `lore reset --last-merged-from` not storing the marks it cleared, so `lore status` still showed a file it had restored as changed.
- `lore-revision`: Fixed `lore reset --dry-run` clearing the marks of the files it would restore, so `lore status` no longer showed changes the working tree still held.
