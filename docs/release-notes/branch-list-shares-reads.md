---
type: feature
---

- `lore-server`: `lore.revision.v1` `BranchList` calls for one repository on one server share the
  mutable store reads of its branches, whatever their `creator` and `include_deleted`. A call made
  while a read runs joins it. A call made less than a window after a read started gets that read's
  result, so a repository's branches are read at most once per window per server process. A list
  that fails as it starts ends every call in the window with the same error. On the AWS store, a
  list page still throttled after its retries ends the list short without an error, and every
  call in the window gets that short listing. A listing can be up to the window older than the
  read alone allows. Forwarded calls share reads on the server that runs them.
- `lore-server`: `[feature] branch_list_share_window_millis` sets the window. It defaults to 1000.
  0 shares only reads that are running. A server whose window is longer than 60000 fails to start.
- `lore-server` metrics: under `lore.revision.v1.revision_service.branch_list.read`, `started`
  counts reads, `callers` counts calls labelled `served` by `new`, `joined_running`,
  `reused_finished` or `kept_error`, `duration` gives each read's time in milliseconds, and `ids`,
  `metadata_entries` and `latest_entries` give the entries each read lists.
