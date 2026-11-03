---
type: improvement
---

- `lore-revision`: a file that `lore sync`, `lore branch switch`, `lore reset` or a merge writes into the working tree
  no longer asks the file system to create the directory it goes in first. That cost every file a failed `mkdir` and a
  stat of a directory that was already there, and on Windows a `CreateDirectoryW` and an open, query and close of the
  directory that every filesystem filter driver also saw. The directory is now created only where the write finds it
  missing, such as below a directory the view excludes.
