---
type: improvement
---

- `lore-server`: A branch push to a server whose immutable store is a composite store fetches the
  parts of the pushed tree and its parents' trees that differ into the local tier ahead of the
  push's verification, rather than one at a time as verification reaches them. A fast-forward merge
  of a push does the same for its comparison of the branch head with the push. A push of a tree the
  server doesn't hold locally no longer waits on each of those reads in turn. The
  log line for a slow, cancelled or failed push reports what each phase's fetching ahead found and
  fetched, in `verify_prefetch` and `merge_prefetch`, and the `prefetch_duration` histogram records
  how long it took.
