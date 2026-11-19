---
type: improvement
---

- `lore-revision`: a sync or `lore branch switch` that deletes whole directories without moving their files elsewhere,
  such as narrowing a view, holds far less memory, however many files they hold, and starts deleting sooner. Files below
  a deleted directory are reported in no fixed order, and the total of files to delete that progress events carry can
  grow while the sync runs. A sync refused for local changes stops checking a deleted directory sooner, and one that
  fails to delete a file below a deleted directory stops once the deletes already running finish.
