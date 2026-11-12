---
type: improvement
---

- `lore-revision`: Staging a directory by walking it, as `lore stage --scan` and `lore --force stage` do, and as `lore stage` does for a directory the tree doesn't hold yet, no longer searches every file it staged before each new file it meets there, so the time no longer grows with the square of the number of new files: 50,000 new files in one directory stage in 0.3 seconds instead of 6.4.
