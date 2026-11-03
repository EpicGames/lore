---
type: fix
---

- `lore-aws`: Fixed `DynamoDB` batch reads resending unprocessed keys immediately and without limit while a
  table is throttled. Each batch of up to 100 keys now waits before resending its unprocessed keys. By default,
  waits start at about 50 ms and double up to about 1 s, and a batch gives up after 6 resends. A read that gives
  up fails rather than returning the items it got. The immutable store answers `SlowDown`, as it does when
  `DynamoDB` throttles a request outright. The lock store reports an internal error.
  `urc.aws.dynamodb.unprocessed_retries.exhausted` counts the reads that give up, labelled by `table_name` and
  `context`. `urc.aws.dynamodb.unprocessed_retries.total` gains buckets at 30, 50 and 100.
- `lore-server`: The AWS immutable store and lock store plugins take an optional
  `dynamodb_unprocessed_keys_retry` table to override these defaults. Its fields are `initial_backoff_ms`,
  `max_backoff_ms`, `max_attempts` and `jitter`. `max_attempts` counts resends.
