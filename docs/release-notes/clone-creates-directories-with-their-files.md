---
type: improvement
---

- `lore-revision`: Under a view, `lore clone`, `lore sync`, `lore sync --view`,
  `lore branch switch`, `lore branch merge`, `lore revision cherry-pick`, `lore revision revert`,
  `lore reset`, `lore link add` and `lore layer add` create a directory the view keeps together
  with what they create in it, rather than ahead of its files: one holding a file the view keeps
  with that file, one holding a directory the view keeps with that directory, and one holding
  neither on its own. This speeds up a clone on NTFS: a clone of 104,553 small files under a view
  with `!/path/**` took 44 seconds instead of 123 on a test volume.
