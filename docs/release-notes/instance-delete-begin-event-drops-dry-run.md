---
type: breaking
---

- `lore-capi`: `lore_repository_instance_delete_begin_event_data_t` no longer has a `dry_run` field, and the
  JSON form of the `RepositoryInstanceDeleteBegin` event no longer has `dryRun`. An event doesn't repeat the
  arguments of the call it belongs to: a caller that needs to know whether a delete is a dry run reads the
  `dry_run` it set in `lore_global_args_t`. Rebuild against the new `lore.h`. A client and the Lore service it
  relays to must come from the same build, or a relayed `instance delete` fails to decode the begin event; restart
  the service after upgrading.
