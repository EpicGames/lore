---
type: feature
---

- `lore-server`: `[environment.config] max_lock_batch` sets the most resources a client puts in one lock, unlock or lock
  status request. The environment response carries it as `max_lock_batch`, and a client splits its lock requests into
  batches of that size. `0` or unset advertises the lock store's limit, 100 for the `local` and `aws` lock stores, and
  any other value the smaller of the two. A client of a server that advertises none batches to 100, as before
