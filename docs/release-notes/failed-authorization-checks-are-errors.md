---
type: fix
---

- `lore-server`: Fixed a failed `CheckUserPermission` call to the auth service reporting a
  repository as not found or not permitted. Only an answer that the caller isn't permitted is
  reported that way. When the auth service can't be reached, rate limits the server or answers
  that its call timed out, the server answers `RESOURCE_EXHAUSTED` on gRPC, `503` on HTTP and a
  slow-down response on QUIC. The `lore` client retries repository lookups and QUIC storage
  operations that get these with backoff. Any other failure of the auth service is an internal error.
- `lore-server`: Fixed a repository lookup that failed on a store fault answering `NOT_FOUND`. It
  answers `INTERNAL`.
