---
type: fix
---

- `lore-revision`: Fixed `lore revision info --delta` listing a revision that moved a link's pin
  as the mount alone. It now also lists each file the pin move changed inside the link, under the
  mount path, as `lore revision diff` lists them but whatever the instance's view, and
  `--metadata` reads those files' metadata from the linked repository.
