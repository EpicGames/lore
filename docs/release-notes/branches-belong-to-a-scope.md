---
type: feature
---

- `lore-server`: every branch belongs to a scope, which the server assigns when it creates the branch and which never
  changes. `lore.revision.v1.BranchCreateRequest` gains `scope`, the id of a live scope of the repository for the new
  branch to join; an unknown or archived scope is refused with `FAILED_PRECONDITION`, and a value that isn't 16 non-zero
  bytes with `INVALID_ARGUMENT`. A create that names no scope, a create through `urc.rpc` and a branch's first push
  included, puts the branch in the scope of its parent, or of the nearest branch among the first 64 of its stack that
  the server holds, and a root branch, or one whose stack holds none of those, in the default scope. A create of a
  branch that exists already, such as one that restores an archived branch, keeps the branch's scope, and naming another
  is refused with `FAILED_PRECONDITION`. The branch's metadata holds the scope's id under the read-only key `scope`
  where it isn't the default scope, and `lore.model.v1.Branch` gains `scope`, which carries it in every
  `lore.revision.v1` create, get, list and delete response. Nothing enforces scopes yet. A server that predates the
  request's `scope` ignores it, and the branch is in the default scope, so upgrade every server, including the peers a
  server forwards `BranchCreate` to, before clients name scopes.
