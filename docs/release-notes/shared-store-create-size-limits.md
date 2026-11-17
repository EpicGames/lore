---
type: feature
---

- `lore-client`: `lore shared-store create --max-size <bytes> --max-capacity <fragments>` (C API:
  `max_size` and `max_capacity` in `lore_shared_store_create_args_t`) sets the store's eviction and
  compaction limits.
