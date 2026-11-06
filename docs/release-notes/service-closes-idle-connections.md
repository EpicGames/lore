---
type: feature
---

- `lore`: `lore service run`, and a program that runs the service through the C API, close a
  connection to a remote once no command has used it for five minutes, so the server releases it.
  Until then the next command reuses it without connecting again. Set how long in seconds with
  `idle_connection_timeout` under `[service]` in the user-level `config.toml`.
