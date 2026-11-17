---
type: breaking
---

- `lore-revision`: An instance over a shared store, and `lore repository gc` on it, collect to the
  limits in the `[store_config]` table of the store's `shared_store.toml`, no longer to the
  `[store]` limits in the instance's own `config.toml`. Move limits raised in an instance's
  `config.toml` to the store's `shared_store.toml`. A `lore_storage_open` handle over a shared store
  also takes the store's limits in place of its `cache_target_bytes` and `cache_target_fragments`.
- C API: `lore_shared_store_create_args_t` gains `max_size` and `max_capacity`, changing its size;
  rebuild against the new `lore.h`.
