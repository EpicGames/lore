---
type: feature
---

- `lore-revision`: `branch merge start --link` and `branch merge abort --link` take the path of a
  link nested in another link. The merge is staged in the repository holding the link, which the
  repositories above it pin, so `branch merge resolve`, `commit`, `commit --link` and
  `branch merge abort` work on it as on a merge of a link of the main repository, and so does
  `branch merge abort --link` of a link holding it. An abort keeps the work staged in a repository
  holding the link. A merge of every link is refused while such a
  merge is in progress.
