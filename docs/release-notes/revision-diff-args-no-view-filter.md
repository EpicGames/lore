---
type: breaking
---

- `lore`: `lore_revision_diff_args_t` gains `no_view_filter`, which lists changes to paths the
  view excludes. The struct changes size: rebuild against the new `lore.h`. A zero-initialized
  struct keeps the current behaviour.
