---
type: fix
---

- `lore-revision`: Fixed `lore --force stage <directory>` staging only what was marked dirty below the directory, so it
  staged nothing for a committed directory deleted from the working tree. A forced stage of a directory walks it as
  `--scan` does and stages everything in it but Lore's own entries, past the ignore file and the view: unchanged files
  are restaged, and what the working tree no longer holds is staged deleted, except what the view leaves out.
  `lore --force stage .` stages the whole working tree. The same holds for a forced stage through the C API.
