---
type: improvement
---

- `lore-client`: `lore clone` and `lore repository create` with a `--vfs` other than `none` use the shared store for the
  remote as if `--use-shared-store` were given, and take `--shared-store-path` without it. An SWFS clone made without
  the flag failed with an internal error
- `lore`: `lore_repository_clone` and `lore_repository_create` use a shared store for a virtual clone where
  `use_shared_store` is `LORE_SHARED_STORE_MODE_INHERIT`, whatever the machine's setting, and refuse
  `LORE_SHARED_STORE_MODE_DISABLED` for one with `InvalidArguments`
