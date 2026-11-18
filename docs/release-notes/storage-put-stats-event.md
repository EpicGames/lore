---
type: feature
---

- `lore-capi`: `lore_storage_put`, `lore_storage_put_resolved`, `lore_storage_put_file`,
  `lore_storage_put_file_resolved` and `lore_storage_upload` send a new
  `LORE_EVENT_STORAGE_PUT_STATS` event at `stats >= 1`, once every item has finished and before
  `LORE_EVENT_COMPLETE`, including when items fail. Its `lore_storage_put_stats_event_data_t`
  reports how many items the call ran, how many of them failed, and what the call's fragment
  writes cost, as the `lore_fragment_stats_data_t` that `LORE_EVENT_REVISION_COMMIT_STATS` carries:
  the fragments produced, deduplicated and uploaded, the bytes uploaded, and the remote queries and
  their hits. A call that runs no item sends none, and `stats = 2` adds nothing for these calls.
