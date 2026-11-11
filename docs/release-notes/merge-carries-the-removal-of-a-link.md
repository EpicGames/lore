---
type: fix
---

- `lore-revision`: Fixed `lore branch merge` failing when the merged branch removes a link whose repository has changes
  to merge, with `Link not found`, with or without a view, and when it renames in case a directory holding a link, with
  `file not found`. The merge carries the removal and leaves out the merge of the link's repository: it doesn't merge
  that repository, except for a link removed inside an out-of-view subtree the merge adopts whole, whose repository it
  merges and then leaves out. A `--link-message` naming a removed link is left out of the merge's commit, and
  `lore branch merge abort` puts a removed link back. A link the merged branch renames in case, or that stands below a
  directory it renames in case, is merged where the rename puts it. A directory the merged branch moves elsewhere
  removes the link inside it and adds it at the merged branch's pin, without the current branch's changes to its
  repository.
