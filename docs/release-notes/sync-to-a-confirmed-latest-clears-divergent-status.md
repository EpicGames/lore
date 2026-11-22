---
type: fix
---

- `lore`: Fixed `lore_revision_graph_branch_sync` leaving a divergent branch latest in place when
  the target is the local latest and the remote confirms it on its history line for the branch,
  so `lore status` reported the branch both ahead of and behind the remote. The verb now clears
  the divergent status.
- `lore-revision`: Fixed `lore sync` leaving a divergent branch latest in place when it syncs to
  that latest and the remote holds it: at the remote latest, or, given a revision, at the number
  the remote holds it at. `lore status` then reported the branch both ahead of and behind the
  remote once the remote moved on. The sync now records the latest convergent.
