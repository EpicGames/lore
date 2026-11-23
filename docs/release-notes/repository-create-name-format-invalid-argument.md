---
type: fix
---

- `lore-server`: Fixed `RepositoryCreate` answering `INTERNAL` when the repository registry rejects the
  repository name. When the registry answers `INVALID_ARGUMENT` with a `google.rpc.ErrorInfo` in the
  domain `lore.error.v1`, a `lore.error.v1.ErrorReason` reason, and `metadata["field"]` naming the
  request's name field (`name` for `RepositoryRegistryService`, `resource_name` for the auth service's
  `RebacApi`), the server answers `INVALID_ARGUMENT` with `Invalid repository name:` followed by the
  registry's message.
