---
type: fix
---

- `lore-revision`: Fixed a `lore sync` or `lore branch switch` that failed to start updating the working tree
  leaving the repository held after the command completed. It logged "Repository has strong reference remaining
  after completion", and a build with debug assertions aborted.
