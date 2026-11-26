---
type: feature
---

- `lore-server` (AWS store): `max_connections` in a store's `http` table, such as `[plugins.aws.immutable_store.http]`,
  sets the most requests each of the store's S3 and DynamoDB clients has in flight at once. A request past the limit
  waits for a connection until its operation's deadline, or for `connection_acquire_timeout_millis` when that is
  sooner. One that finds none released by then is not sent, and the
  store backs off from it as from throttling. The operation metrics label it `ConnectionLimit` in `aws_code`. One whose
  wait cut its attempt's time short, and that is then not answered in time, is labelled `ConnectionLimitTimeout` rather
  than `Timeout`, and the store backs off from it too. `urc.aws.http.connection_wait` records how long each request
  waited, labelled with its `store` (`immutable`, `mutable` or `lock`) and `service` (`s3` or `dynamodb`). A limit of
  `0` stops the store from starting. Unset, which is the default, there is no limit
