---
type: feature
---

- `lore-client`: `lore auth info --with-claims` prints the claims of the current repository's
  authorization (access) token as JSON, without printing the token. On a deployment that does not
  exchange tokens, this is the login token. The claims show the grants the token carries, but the
  server can grant more than they show.
- `lore-capi`: `lore_auth_local_user_info_args_t` has a new `with_claims` field, and
  `lore_auth_identity_event_data_t` has a new `claims` field. With `with_claims` set, the call
  requires a repository and emits `LORE_EVENT_AUTH_IDENTITY` with the token's claims as a JSON
  object, and fails with `NotSupported` when the token is not a JWT. The event's `token` is empty
  unless `with_access_token` is also set.
