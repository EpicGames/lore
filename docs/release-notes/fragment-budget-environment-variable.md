---
type: feature
---

- `lore-storage`: The `LORE_FRAGMENT_BUDGET_MIB` environment variable sets, in MiB, how much
  fragment data a process holds in memory at once across its writes, its streamed reads and its
  reads into a file, such as content read and waiting to be uploaded. A read that returns content
  in memory isn't bounded by it. A process reads it once, the first time it needs the budget. A
  command the service process runs uses the service's own budget, set from the environment the
  service started in. Unset or blank, the budget is 1024 MiB, as before. A value below 4 is
  raised to 4 and a value above 524287 is lowered to 524287, and a value that isn't a whole
  number, such as a negative one, leaves the default in place. Each of these logs a warning.
