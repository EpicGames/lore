---
type: fix
---

- `lore-transport`: Fixed a stored login that has expired and can no longer be refreshed failing
  with `NotAuthorized` (17) instead of `NotAuthenticated` (16). A refused repository authorization
  still fails as `NotAuthorized`. An unreachable auth service, or a refresh that could not be sent,
  fails as `Disconnected` (28) and keeps the refresh token.
- `lore`: Fixed `lore auth info` and `lore auth logout` failing on such a login. `auth info`, and
  `lore_auth_local_user_info` and `lore_auth_user_info` without an auth endpoint, refresh an
  expired login first, unless the call supplies a token.
