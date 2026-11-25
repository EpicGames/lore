---
type: fix
---

- `lore-server` (AWS store): Fixed a misconfiguration, such as `AccessDenied` or a missing table, surfacing only once the
  whole retry ladder was spent, because most store operations took every S3 or DynamoDB failure for `SlowDown`. An
  immutable or mutable store operation whose call fails with anything other than throttling, a server error or a
  transport failure now fails as an error
- `lore-server` (AWS store): Fixed a lock store query or lock status check that `DynamoDB` shed failing as an internal
  error, including a status check whose batch read still had keys unprocessed after its resends, and a lock or unlock
  it shed failing at once. The lock store answers a shed call with `SlowDown`, and a lock, unlock, query or status
  check is asked again with back-off while it is shed, failing with `SlowDown` once the retries run out
- `lore-server` (AWS store): Fixed obliterating a payload with more than 1,000 versions of its S3 object reporting success
  with the versions past the first listing page left in place. The obliteration reads the version listing page by page
  and deletes every version, and a listing that cannot be read to its end fails the obliteration as an error
- `lore-server` (AWS store): Fixed an obliteration step shed by S3 or DynamoDB after the obliteration took its mark
  failing with `SlowDown`, after which the retry found the mark and reported success with the payload still in place.
  Each such step is retried while it is shed, and fails as an error once the retries run out. An obliteration that
  finds the content already being obliterated also deletes the caller's association, which it left in place
- `lore-server` (AWS store): Fixed a mutable store compare-and-swap, such as a branch pointer update, reporting its own
  write as a conflicting one when sent again after its answer was lost. A swap that finds the value it writes already
  in place reports the swap as applied
