---
type: feature
---

- `lore`: The low-level revision graph API works on a repository's branches and revisions through a
  storage handle and a repository ID, with no repository instance, so a pipeline reads a branch's
  latest revision or lists branches without cloning the repository or reading Lore's internal
  mutable-store keys. Each call has an `_async` variant. It consults the remote, where the storage
  handle has one, then the local store, or one of them alone as `globals.local`, `globals.remote`
  and `globals.offline` select, on the call or when the storage handle was opened. It reports each
  location's answer in its own records.
  `lore_revision_graph_revision_resolve` resolves a revision from its hash, or from a branch and a
  revision number or the branch's latest. `lore_revision_graph_branch_resolve` resolves a branch
  name to its branch, with its latest revision and the repository instances registered in the local
  store whose current branch it is. `lore_revision_graph_branch_list` lists the branches, archived
  ones too on request, and follows each branch the local store holds with the instances on it.
