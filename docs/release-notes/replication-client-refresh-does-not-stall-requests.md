---
type: fix
---

- `lore-server`: Fixed replicated store and replica requests stalling while the replication
  client is refreshed or reconnected. A refresh waited for every request in flight and held back
  new ones until then, so one slow request could block the store until it finished.
  Requests in flight now finish on the old client, and new requests use the new client as soon
  as it connects.
- `lore-server`: Fixed delayed reconnection after a connection to a replication peer failed.
  Reconnection sometimes happened only after further requests failed. A request that started
  during a refresh could run on the new client while recording the old one, so its failure was
  ignored as already handled.
