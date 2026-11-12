---
type: improvement
---

- `lore-revision`: Marking a directory's files with `lore file dirty <directory>`, and staging the dirty files of one with `lore stage <directory>`, take time that no longer grows with the square of the number of files in the directory, except below a link: 50,000 new files in one directory mark in 0.3 seconds instead of 18, and stage in 0.4 seconds and 3 seconds of processor time instead of 1.9 seconds and 182. Marking them takes 48 MB at its peak instead of 41. A failure to read the children of a directory of more than eight entries fails `lore file dirty`, where it was taken as no child.
