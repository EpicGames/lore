---
type: feature
---

- `lore-revision`: A link both branches of a merge changed, in its pin, branch, flags or mounted
  folder, is a conflict the merge holds open instead of an error. `lore branch merge start` names
  it, on a dry run too, `lore branch merge resolve mine|theirs <link path>` writes the chosen side's
  whole row and realizes it at the mount, plain `lore branch merge resolve` keeps this branch's
  row unless one is already chosen, `lore commit` refuses while any is unsettled, and
  `lore branch merge abort` restores it. `lore branch merge abort --link <path>` keeps the rows of
  the other links.
  A row only the merged branch changed lands with its branch and flags as well as its pin.
- `lore-client`: `lore branch merge conflicts list` lists the files and links an open merge still
  holds, with what differs between the two sides of each link and the message of the revision each
  side pins.
- C API: `lore_branch_merge_conflicts_list` and `lore_branch_merge_conflicts_list_async`, and the
  `LORE_EVENT_BRANCH_MERGE_LINK_PIN_CONFLICT` and `LORE_EVENT_BRANCH_MERGE_LINK_PIN_RESOLVED`
  events, sent by merge start and every merge resolve.
