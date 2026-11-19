---
type: improvement
---

- `lore-revision`: Under a view or an ignore file, the walks over the working tree and the tree spend less time in the filter: `lore status --scan`, `lore sync --force --reset`, `lore stage`, `lore unstage`, `lore file dirty`, `lore file info`, and the comparisons of trees behind `lore status`. Under a view of about 6,300 lines over a tree of 450,000 files, a no-op `lore status --scan` or `lore sync --force --reset` takes a sixth to a quarter less time on 4 or 8 cores.
- `lore-revision`: `lore status` lists a staged link that replaces a directory as added, beside the directory's delete, where the ignore file names the link but re-includes a path below it. It listed the delete alone.
