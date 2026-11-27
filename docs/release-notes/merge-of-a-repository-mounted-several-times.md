---
type: fix
---

- `lore-revision`: Fixed the merge of a repository one repository mounts several times, which was
  refused or left the mounts at different pins where its merge conflicted. The repository is
  merged once, each conflict is marked at the mount holding it, and a resolve, an abort,
  `lore branch merge start --link`, `lore branch merge abort --link` and `lore commit --link`
  through any mount act on every mount, as does resolving a link conflict on one of its rows. A
  link the merged branch adds to the repository lands at the merge.
