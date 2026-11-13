---
type: fix
---

- `lore-revision`: Fixed an operation on an SWFS instance, started right after another one ended as commands such as
  `lore file diff` do, failing with `Failed to freeze`. SWFS returns from a thaw before it has finished, and a thaw
  is now waited for, so the next freeze starts once SWFS has finished it
- `lore-revision`: Fixed commands that ran at once on the same SWFS instance thawing the mount under each other, or
  failing with `Failed to freeze`. A command now waits until the one holding the instance has finished
- `lore-revision`: Fixed an SWFS instance that was frozen when the service mounted it, such as after the service ended
  mid-operation, still being frozen once it was ready. Its thaw is now waited for as well
