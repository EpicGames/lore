---
type: improvement
---

- `lore-revision`: A command on an SWFS instance waits up to 3 seconds for SWFS to thaw the
  instance when the command finishes. When the thaw can't finish in that time, most often because
  too much I/O reaches the instance at once, the command fails and says to wait until that I/O has
  stopped and then run another command on the instance, which finishes the thaw before it begins.
