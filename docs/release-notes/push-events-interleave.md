---
type: breaking
---

- `lore-revision`: the push events `lore_branch_push_fragment_begin_event_data_t`, `..._fragment_progress_...`,
  `..._fragment_end_...` and `lore_branch_push_branch_create_end_event_data_t`, and the revision update and revision push
  update events, start with the `repository` and `branch` they belong to. Branches push at once, so the events of
  different branches interleave, in no fixed order between branches. Rebuild against the new `lore.h`, and track push
  progress per repository and branch.
- `lore-client`: `lore push` shows the fragments of every branch it pushes at once in one progress bar.
