---
type: feature
---

- `lore-server` (AWS store): `connection_acquire_timeout_millis` in a store's `http` table bounds how long a request
  waits for a connection under `max_connections`. A request that finds none released in time is not sent, and the store
  backs off from it as from throttling, reporting `SlowDown` once its back-off is spent. Unset, which is the default, a
  request waits until its operation's deadline. `0` sends a request only when a connection is free.
- `lore-server` (AWS store): `s3_max_connections` and `dynamodb_max_connections` under `[plugins.aws.immutable_store]`
  set the limit of the immutable store's S3 and DynamoDB clients apart, in place of the one its `http` table sets.
- `lore-server` (AWS store): `lore.aws.http.requests_in_flight` counts the requests each S3 and DynamoDB client has in
  flight, with or without a limit, and `lore.aws.http.max_connections` reports each client's limit when it has one. Both
  are labelled with the client's `store` and `service`, as `urc.aws.http.connection_wait` is. Each request in flight
  holds a connection of its own, so the count over the limit is how close a client runs to its limit
