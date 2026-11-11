---
type: breaking
---

- `lore-credential`: a command relayed to the Lore service without a `LORE_AUTH_STORE` uses the secure store, not
  the store the service's own environment names. A program served by a service started with
  `LORE_AUTH_STORE=fallback` sets `LORE_AUTH_STORE=fallback` itself, or, for the `lore` command, `[auth] store`.
- `lore-credential`: on Unix, a link in place of a token store file is refused rather than followed. Replace the link
  with the file.
- `lore-client`: a `LORE_AUTH_STORE` naming neither `secure` nor `fallback`, which read as the secure store, stops
  every `lore` command, as does a user-level `config.toml` that can't be read, isn't TOML, or holds an `[auth]`
  table that doesn't parse. Correct the value or the file.
- `lore`: a call relayed to the Lore service carries one more variable, so a client and a running service from
  either side of this release misread each other's calls. End a running Lore service's process after upgrading.
