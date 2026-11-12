---
type: improvement
---

- `lore-revision`: A FUSE virtual clone answers requests that arrive at the same time with less
  waiting between them: each reads what the mount serves without taking a lock the others share.
