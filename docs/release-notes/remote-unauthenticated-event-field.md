---
type: feature
---

- `lore-capi`: `remote_unauthenticated` on `lore_revision_sync_target_event_data_t` and
  `lore_repository_status_revision_event_data_t` is set where no valid login reached the remote: none is stored for its
  server, it expired and could not be refreshed, or the remote rejected the token presented. In the status event it is
  also set where `remote_available` is zero, since a login refused at the connect leaves the remote unreachable
