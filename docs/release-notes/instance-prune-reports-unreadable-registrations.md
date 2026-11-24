---
type: breaking
---

- `lore`: `lore_repository_instance_prune`, and `lore_repository_clone` over a shared store, report each instance
  registration they remove whose data is missing from the local store or malformed as a
  `LORE_EVENT_REPOSITORY_INSTANCE` event with `stale` 4, also under `globals.dry_run`, where nothing is removed. Such
  an event names no instance: its `instance_id`, `branch` and `revision` are zero, and its `path` and `branch_name`
  are empty. A consumer that reads an instance from each such event must skip one with `stale` 4.
- `lore-revision`: A read of metadata of a version this build doesn't read, such as metadata a later version of Lore
  wrote, fails with `NotSupported` (9) rather than `Internal`. A call whose errors include `NotSupported` completes
  with that code where it reads such metadata, and any other call still completes with `Internal`. A consumer that
  handled `Internal` for such a read handles `NotSupported` too. `lore repository instance prune` keeps an instance
  registration of such a version.
- `lore-client`: `lore repository instance prune` prints "Pruned a registration whose data is missing from the local
  store or malformed" for each such registration, and counts it in its total.
