---
type: fix
---

- `lore-revision`: Fixed `branch merge abort --link` refusing a link merged with
  `branch merge start --link` and left uncommitted, once the merge of another link followed it.
- `lore-revision`: Fixed `branch merge abort` failing with "No merge is in progress" after merges
  of links alone made with `branch merge start --link --no-commit` and no conflict, and throwing
  away what else was staged after such a merge with a conflict. It aborts each link's merge and
  keeps the rest.
- `lore-revision`: Fixed `branch merge abort --link` leaving a link staged once it is back at the
  pin the current revision holds, a link held by a linked repository and one whose mount the view
  leaves out included, and leaving a merge of links alone in progress after the abort of its last
  link.
- `lore-revision`: Fixed `commit --link` of a link merged with `branch merge start --link
  --no-commit` leaving its merge in progress, so that `branch merge abort` moved the link's pin
  back behind the commit. The commit ends a merge of links alone with its last link.
- `lore-revision`: Fixed `branch merge start --link` on a merge revision recording that merge's
  second parent and metadata on the revision it commits, and a merge of every link stopped by a
  conflict in a link, on a merge revision, throwing away on `branch merge abort` what was staged
  during it.
