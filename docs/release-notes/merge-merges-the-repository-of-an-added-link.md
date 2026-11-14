---
type: fix
---

- `lore-revision`: Fixed a merge of a branch that adds an auto-following link pinning it at a
  revision of the merged branch's linked counterpart, so the target branch's counterpart never got
  the branch's changes and the next push of a commit inside the link failed. `lore branch merge
  start` and `lore branch merge into` merge the linked repository into the target branch's
  counterpart and pin the link at the result. A conflict stops `merge start` with the markers in
  the mount, and is refused by `merge into`. A link to a repository the target branch links
  elsewhere already keeps that link's pin. `--ignore-links` warns of each such link it leaves
  unmerged.
