---
type: feature
---

- `lore`: The low-level revision graph API works on a repository's branches and revisions through a
  storage handle and a repository ID, with no repository instance, so a pipeline reads a branch's
  latest revision, lists, creates or archives branches, reads and edits a branch's metadata, syncs
  a branch's local latest, or pushes a branch, without cloning the repository or reading Lore's
  internal mutable-store keys. Each call has an `_async` variant. It consults the remote, where the
  storage handle has one, then the local store, or one of them alone as `globals.local`,
  `globals.remote` and `globals.offline` select, on the call or when the storage handle was opened.
  It reports each location's answer in its own records, except a branch sync and a branch push,
  which span both locations. A sync looks its target up at the remote where the call consults it,
  writes the local store, and reports one synced event. A push reads the local store and writes
  the remote: `globals.local` and `globals.offline` fail it with `InvalidArguments`,
  `globals.remote` means the same as no flag, a storage handle without a remote fails it with
  `NoRemote`, and it reports one pushed event.
  `lore_revision_graph_revision_resolve` resolves a revision from its hash, or from a branch and a
  revision number or the branch's latest. `lore_revision_graph_branch_resolve` resolves a branch
  name to its branch, with its latest revision and the repository instances registered in the local
  store whose current branch it is. `lore_revision_graph_branch_list` lists the branches, archived
  ones too on request, and follows each branch the local store holds with the instances on it.
  `lore_revision_graph_branch_create` creates a branch from a revision, or from a branch's latest
  revision or a revision number on it, with the same ID, name and parent branches at each location,
  the remote first. It refuses a name a live branch holds, and a starting point that differs between
  the locations, before it writes anything. `lore_revision_graph_branch_archive` archives a branch
  at each location, the remote first, and keeps the branch reachable by its ID. It refuses a
  protected branch and the default branch, and refuses a branch a live repository instance
  registered in the local store has as its current branch before it changes either location.
  `lore_revision_graph_branch_sync` moves a branch's local latest to a revision, or to the remote
  latest, where it fast-forwards, or anywhere with `force`, and only while the local latest is the
  one the caller expects where it names one. It records the branch in sync with the remote where
  the target is the remote latest, and records the local latest as divergent unless the remote
  shows that its history for the branch holds the target. It brings in a branch the remote holds
  where the local store lacks its metadata or doesn't map its name to it, and reports the local
  latest it found when it fails. `lore_revision_graph_branch_push` publishes a branch's local
  history to the remote, creating the branch there on its first push, with `force` and a
  fast-forward merge onto a moved remote latest as options. It publishes the branch alone: each
  revision of a linked repository a revision links must be on the remote already, and is checked
  there before anything is pushed. It also uploads the revisions the remote lacks on the
  second-parent line of each merge revision, and moves no other branch's remote latest. It moves
  the local latest only from the value it pushed, so a revision committed during the push stays,
  and reports the revision published, the remote latest and
  the local latest, also when it fails once it has read the remote latest. After a
  `BranchAdvanced` failure, a forced sync to that remote latest recovers.
- `lore`: `lore_revision_graph_branch_metadata_get` reports a branch's metadata entries, or the
  one entry a key names, at each location. `lore_revision_graph_branch_metadata_set` sets and
  removes entries at each location and refuses the keys Lore maintains itself. Where another
  writer changed the metadata meanwhile, it keeps that writer's entries for the keys it doesn't
  edit, so the last writer wins key by key. Where a set consults both locations, it reaches the
  local store only after the remote succeeds.
