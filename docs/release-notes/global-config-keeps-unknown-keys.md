---
type: fix
---

- `lore-revision`: Fixed commands that change a setting in the user-level `config.toml` dropping the keys and
  tables they don't know, such as a setting that a newer version of Lore added. These include
  `lore service set-executable` and a clone that records a default shared store.
