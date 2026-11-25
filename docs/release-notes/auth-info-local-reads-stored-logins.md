---
type: fix
---

- `lore-client`: Fixed `lore auth info --local` and `--offline` connecting to the repository's
  remote, and failing with `Disconnected` when the remote could not be reached. They now report
  the stored logins whose tokens the remote's domain accepts, without connecting, and fail with
  `NotAuthenticated` when the current user has none. A login stored by a release whose tokens did
  not record their accepted domains does not count. Run `lore auth login` again to use it with
  `--local` or `--offline`. With `--with-access-token` or `--with-claims`, `--local` connects as
  the command does without it, and `--offline` fails with `NoRemote`.
- `lore-client`: Fixed `lore auth info` without `--identity` reporting the first stored login as
  the current user rather than the identity the repository records, which other commands act as.
- `lore-capi`: Fixed `lore_auth_local_user_info` with `globals.local` or `globals.offline` set and
  an empty `auth_endpoint`, and `lore_auth_user_info` with empty `user_ids` and either flag,
  connecting to the repository's remote. They resolve the identities from a supplied token and
  from the stored logins the domain of the repository's remote accepts, and fail with
  `NotAuthenticated` when the current user has no such login.
