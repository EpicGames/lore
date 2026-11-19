---
type: fix
---

- `lore-server`: Fixed `RepositoryCreate` answering `INTERNAL` when the repository registry rejects the
  repository name's format. It answers `INVALID_ARGUMENT`, with `Invalid repository name:` followed by
  the registry's message, which states the expected format. This covers registry rejections whose
  message starts with `resource_name must be`.
