---
type: fix
---

- `lore-server`: Fixed gRPC request handlers that stopped part way through a write when the request handler timeout
  elapsed. When the timeout elapses or the client disconnects, a handler is now cancelled rather than dropped. It
  completes any write it has started and stops at the next point where stopping leaves no partial write. A handler that
  stops because of the timeout still answers `CANCELLED` with "Request handler timeout exceeded". A handler that
  reaches no such point before it finishes now runs past the timeout to its end.
- `lore-server`: Fixed the legacy `BranchDiff` and `RevisionDiff` RPCs and
  `lore.thin_client.v1.ThinClientService/RevisionDiff` going on computing a diff for hours after the client had gone.
  The base search, the tree walks and the per-conflict history walks now stop once the request is cancelled.
- `lore-server`: A cancelled push stops before it stores the new latest revision: during fragment verification, before
  the revision is rewritten, before a fast-forward merge is applied and before each attempt to store the latest
  revision. Once the latest revision is stored the push always completes, including its notification and post-handler
  hooks. The push metrics record it with the `cancelled` outcome.
- `lore-server`: Branch create and delete, repository create and delete, lock, unlock and obliterate answer `CANCELLED`
  without writing anything when their request is cancelled before the first write. Once that write starts, the
  operation and its notification run to completion.
- `lore-server`: The streaming storage RPCs, legacy and v1, and replication put stop taking new messages once their client
  has gone. Items already started finish. `lore.repository.v1.RepositoryService/RepositoryList` loads nothing for a
  request cancelled before it starts loading.
- `lore-server`: `lore.thin_client.v1.ThinClientService/RevisionTree` and the legacy `RevisionTree` stop walking the
  tree once their request is cancelled.
- `lore-server`: `lore.thin_client.v1.ThinClientService/RevisionDiff` resolves the base of a 3-way diff once instead of
  twice, so the history search before its first item runs once.
- `lore-server`: `lore.revision.v1.RevisionService/RevisionList` and the legacy `RevisionList` stop walking history once
  their request is cancelled, as does resolving a revision by number or by an `~` offset.
