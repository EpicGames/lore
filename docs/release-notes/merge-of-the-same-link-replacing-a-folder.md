---
type: fix
---

- `lore-revision`: Fixed `lore branch merge` of two branches that each replaced a folder with a link to the same
  repository conflicting at the link with no resolve allowed, leaving only an abort. The merge now lands the one link,
  and where the two links differ in pin, flags or mounted subtree, it conflicts at the link's row, which
  `lore branch merge resolve mine` or `theirs` settles. The same holds for a link both branches added at one path.
  A push, cherry-pick, revert or `lore branch merge into` still conflicts where two such links differ.
