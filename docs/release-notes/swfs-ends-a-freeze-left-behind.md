---
type: fix
---

- `lore-revision`: Fixed every command on an SWFS instance failing to freeze it, until the
  instance was mounted again, after a command ended without thawing it. A command that fails while
  finishing now thaws the instance before it reports the failure, and the next command ends a
  freeze still left behind before it begins.
