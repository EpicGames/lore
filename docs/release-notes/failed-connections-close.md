---
type: fix
---

- `lore-transport`: Fixed a connection to a remote that had failed, or that another command
  connecting to the same remote at the same moment had replaced, staying open with its keep-alives
  until the process exited. A process that runs many commands, such as `lore service run` or a
  program embedding Lore, kept each such connection's storage connections open.
