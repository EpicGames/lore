---
type: improvement
---

- `lore-revision`: `lore clone` hands a view's files to its writers in the order of the tree,
  each directory's files before the directories below it, except that when the writers are about
  to run out of work it hands them the files it has already found that come first in the tree,
  rather than wait for the next ones in order. It still holds no more than a fixed number of files
  it has found and not yet written. On NTFS a large clone no longer slows down after its first few
  hundred thousand files: on a test volume a clone of 1,197,646 files took 452 to 584 seconds
  instead of 952 to 1,213, and on an EBS volume a clone of a 9.35 million file view reached
  1,000,000 files in about 990 seconds instead of 1,620.
