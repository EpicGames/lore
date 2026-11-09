---
type: feature
---

- `lore-server`: `ThinClientService.RevisionTree` attributes each entry to the revision that last
  changed it when the request sets `include_last_revision`, and the header's
  `last_revision_included` says so. The entry's `last_revision_index` names a `TreeRevision` the
  stream sends before it, carrying that revision's signature, identifier, number, commit message,
  timestamp and committer, and is 0 for an entry the server can't attribute. An entry under a link
  is attributed in the linked repository. A read for attribution that the store refuses as
  overloaded ends the stream with `RESOURCE_EXHAUSTED`. Without the flag, no entry is attributed
  and no `TreeRevision` is sent.
- `lore-server`: `ThinClientService.RevisionTree` names the repository each entry's content is in,
  as `RevisionDiff` does: a link, and an entry in a linked repository, carry in
  `link_repository_index` the index of a `DiffPartition` the stream sends before it, and an entry
  in the requested repository carries 0. Each `TreeRevision` names the repository its revision is
  in the same way.
