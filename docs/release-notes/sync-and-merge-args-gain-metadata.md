---
type: breaking
---

- `lore`: `lore_revision_sync_args_t` and `lore_branch_merge_start_args_t` gain `metadata_keys`,
  `metadata_values` and `metadata_formats`, changing their size: rebuild against the new `lore.h`.
  A zero-initialized struct keeps the current behaviour.
