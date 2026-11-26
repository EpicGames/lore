---
type: breaking
---

- `lore-client`: `lore clone` takes `--max-capacity <fragments>` and `--max-size <bytes>`, the
  limits the new instance's store holds before eviction and compaction, written into its `[store]`
  config in place of the defaults, and enforced by the clone's own store from the start unless
  `--no-gc` is given. Over a shared store, whose limits are set in its `shared_store.toml`, and with
  `--no-tracking`, whose stores are kept in memory, the options have no effect, and the clone warns.
- `lore`: `LoreRepositoryCloneArgs` gains `max_capacity` and `max_size`, where 0 takes the default.
  The struct changes size, so a program that links Lore must be rebuilt against the new `lore.h`,
  with the new fields set or zeroed.
