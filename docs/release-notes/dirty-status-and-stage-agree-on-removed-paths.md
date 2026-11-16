---
type: fix
---

- `lore-revision`: Fixed `lore file dirty` of a tracked file replaced by a directory the ignore file names, or of the directory holding it, marking nothing. It marks the file deleted and leaves the directory out, as `lore status --scan` reports it and `lore stage` stages it.
- `lore-revision`: Fixed `lore stage` of a tracked file replaced by a directory that an ignore rule naming files and directories alike leaves out, such as `Intermediate`, staging nothing. It stages the file deleted, as `lore status --scan` reports it.
- `lore-revision`: Fixed `lore file dirty` of a path below a tracked file replaced by a directory marking nothing, and a sync or branch switch that carries such a mark warning that the file is not a directory. The mark records the file deleted and the directory beside it added, with the path below it.
- `lore-revision`: Fixed `lore stage` without `--scan` of a file replaced by a directory, or a directory replaced by a file, whose change `lore file dirty` marked, staging the new node and leaving the old one's deletion unstaged, so `lore commit` stored a revision holding both under one name. The old node is staged deleted beside the new one, as `lore stage --scan` stages them.
- `lore-revision`: Fixed `lore status` of such a change, marked or staged, reporting the replaced file as moved onto itself, `V f -> f`, or a deletion of the wrong kind beside the right one. It reports the replaced node deleted and the new one added.
- `lore-revision`: Fixed `lore status --scan <directory>` reporting a directory deleted where it was only marked added, by `lore file dirty` or a scan, and then removed from the working tree. Its marks are dropped, as a scan of the whole tree drops them.
- `lore-revision`: Fixed `lore stage <directory>` without `--scan`, of a directory removed from the working tree and marked deleted, staging each path below it deleted and leaving the directory itself unstaged. It stages the directory deleted whole, as `lore stage --scan <directory>` does.
