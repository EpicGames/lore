---
type: breaking
---

- `lore`: A client and the Lore service now compare the schemas of the commands and events their
  builds exchange before a command, and the service refuses a client of another build, which
  fails with `Lore service unavailable` (exit code 32) instead of the service misreading the
  command. `lore service stop` still stops a service of another build. A service from an earlier
  release can't read a command or a stop from this one, and one from this release can't read a
  command from an earlier client: after updating, stop a running service with the `lore` it came
  from, or end its process.
