---
type: fix
---

- `lore-revision`: Fixed `lore reset --revision` and `lore reset --last-merged-from` reading the marks of another file than the one at the path they reset, after a file was deleted and another added, or across a merge. Both refused a path because an unrelated file was staged, and wrote over a staged edit of the path or over a staged add the current revision lacks. `lore reset --revision` also dropped the marks of an unrelated edited or added file, which `lore status` without `--scan` and `lore stage .` then missed.
- `lore-revision`: Fixed `lore reset` of a path whose file was marked moved elsewhere clearing the move's mark and writing over a file staged as added at the path, and discarding a staged addition that took the place of a file marked deleted, with or without `--revision`.
