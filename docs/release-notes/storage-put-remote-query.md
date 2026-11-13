---
type: feature
---

- `lore-capi`: The items of `lore_storage_put`, `lore_storage_put_file`,
  `lore_storage_put_resolved` and `lore_storage_put_file_resolved` have a new `remote_query`
  field, off by default. With it and `remote_write` set, the put asks the remote whether it already
  holds each fragment before compressing and uploading it, and uploads none it holds: a machine that
  stores content the remote already has sends no payload for it, whatever its local store knew. With
  `local_cache` set, a fragment the remote holds is still compressed for the local copy, and only its
  upload is spared. It costs a round trip per fragment the local store does not know as stored
  remotely, so set it where the remote is likely to hold most of the content. A fragment the local
  store knows under another context the remote holds is copied from there first, and asked about
  only if the remote refuses the copy. The field sits in padding the items already had, so their
  size and the offsets of their other fields are unchanged. Zero the whole item with `memset`, or
  give it static storage, before setting its fields: a non-zero padding byte from a caller built
  against an older header turns the query on.
- `lore-capi`: `lore_fragment_stats_data_t`, which `LORE_EVENT_REVISION_COMMIT_STATS` carries, has
  three new fields after the existing ones: `remote_queries`, `remote_query_hits` and
  `remote_query_unsent_bytes`, which count the fragments a write asked the remote about, those the
  remote held, and the payload bytes not sent for them.
- `lore-server`: A metadata request for a fragment the server does not hold is logged at `debug`,
  since a put that asks the remote first makes one for each fragment the server lacks.
