---
type: fix
---

- `lore-revision`: Fixed push, merge and restore retrying fragment queries immediately and indefinitely when the
  server answered `SlowDown`. A throttled batch now waits for a back-off before retrying. The query gives up after
  10 consecutive rounds with no answers, or after 50 throttled rounds in total.
- `lore-revision`: Fixed the error reported when fragment queries and uploads run out of retries on `SlowDown`.
  They now fail with `LORE_ERROR_CODE_SLOW_DOWN` instead of an internal error.
