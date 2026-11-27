---
type: feature
---

- `lore-capi`: `lore_repository_rename` and `lore_repository_rename_async` rename a repository on
  the remote, which keeps its ID. `lore_repository_rename_args_t` takes the new `name`, and a
  `repository_url` that is a URL, a name or an ID on the remote of the repository at
  `repository_path`, or empty for the repository of that instance. The call fails with error
  code 9 (not supported) when the remote cannot rename repositories, and with the new error
  code 54 (rename incomplete) when the remote can have stopped the rename after its checks
  passed. Make the same rename again by the repository URL and ID the error names to complete
  it, or rename the repository back to its old name to abandon it, as when the repeat fails
  because another repository holds the new name.
