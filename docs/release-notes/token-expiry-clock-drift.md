---
type: fix
---

- `lore-credential`, `lore-transport`, `lore-revision`: Fixed stored authentication and
  authorization tokens failing when the client clock is skewed ahead of or behind the issuer's
  clock. Tokens obtained through login, refresh, or exchange now remain valid for their actual
  lifetime regardless of clock offset, refresh up to 10 seconds before expiry, and are reported
  accurately by `lore auth info` (`sessionValid`). Existing stored logins adopt the fix after
  their next login or refresh; tokens supplied directly via `--token-type lore` or
  `--identity-token` continue to expire by their `exp` claim.
