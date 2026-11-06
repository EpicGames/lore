---
type: breaking
---

- `lore-storage`: These storage requests fail with `NotAuthenticated` (16) or `NotAuthorized` (17)
  when the server refuses them, where they failed with `NotConnected` (29): a fragment read, a key
  resolve, a key a resolved write publishes or retracts in a request of its own, a C API storage
  metadata read or obliterate, and a C API storage copy the server refuses as unauthenticated or
  whose fallback upload it refuses. The error keeps the server's reason, where it gives one, on its
  trace. Scripts and C API callers that test a failed storage request for 29 should test for 16 and
  17 as well.
- `lore-storage`: A fragment read or key resolve the server refuses is retried once, at once, on a
  fresh session, since an expired token is refused and a fresh session can carry a fresh one. It
  was retried five times, with back-off between attempts.
- `lore-storage`: `lore file audit` reports a line as link-unavailable when the server refuses to
  read the content of its earlier version in a linked repository, and the instance holds that
  linked revision's state. Such an audit failed with `NotConnected` (29).
