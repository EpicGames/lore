---
type: improvement
---

- `lore-aws`: concurrent requests for the same fragment share one `DynamoDB` read on the AWS immutable store. This
  covers association checks in put, copy, get and metadata lookups, and state reads in put and metadata lookups. A write
  by the same server to the association or the state ends the shared read, so a request to that server that starts
  after the write still sees it. A write by another server does not end it.
  `urc.store.immutable.aws.shared_reads` counts these reads by `read` (`exists` or `state`), `role` (`leader`
  for a request that read for itself, `follower` for one that shared a read in flight) and `context` (the operation)
