---
type: fix
---

- `lore-revision`: Fixed the first commit on a branch that `lore branch create` made after `lore link update`, or over
  nested links, failing with `Branch has been advanced by another instance`. A link moved to a revision on its
  parent's branch keeps following that branch, so the new branch needs no revision to repin it
- `lore-revision`: Fixed `lore branch create` over a link pinned to another branch leaving the working copy behind the
  new branch, failing with `Failed to deserialize revision metadata` when changes were staged, and leaving a link that
  a staged change removes or remounts on its old branch
- `lore-client`: Fixed `lore link update` on a link whose repository has not moved printing only
  `Link is already up to date`. It names the link and says it was not pinned
