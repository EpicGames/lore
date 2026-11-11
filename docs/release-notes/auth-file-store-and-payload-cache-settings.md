---
type: feature
---

- `lore-client`: an `[auth]` table in the user-level `config.toml`. `store = "fallback"` keeps the token store's key
  in a file its owner alone can read instead of the system's secure store, for machines where nothing can answer a
  secure-store prompt, and `store = "secure"` keeps it in the secure store; any other value is an error. `path` moves
  the token store, a relative `path` is relative to the directory of `config.toml`, and `fallback` needs one, or
  `LORE_AUTH_PATH`. `LORE_AUTH_STORE` and `LORE_AUTH_PATH` override them with the same values, and a blank variable
  is unset. Only the `lore` command applies the table.
- `lore-credential`: on Unix, a new token store directory is its owner's alone.
- `lore-revision`: `[store] cache_payloads` in an instance's config sets whether the content of files fetched from
  the remote, and of files written, is kept in the store.
