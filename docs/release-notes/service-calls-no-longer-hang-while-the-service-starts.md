---
type: fix
---

- `lore`: Fixed `lore_service_start`, `lore_service_stop` and `lore_service_status` hanging
  indefinitely in a program that runs the Lore service, when called while that service was still starting
  after it began to listen on its socket, or at the moment the program started it. They now act on
  that service from the moment it has claimed the socket, as they do once it runs: a start
  succeeds, a status reports it running in that program, and a stop is kept and ends the service
  once it has started.
