---
type: improvement
---

- `lore-server` metrics: `BranchPush` records metadata blob reads for changed file metadata blocks during fragment
  verification. Under `lore.branch_push`, `metadata_reads` counts these reads per push, and
  `peak_metadata_reads_in_flight` records the maximum number of concurrent reads. The log line for slow, cancelled or
  failed pushes includes the same counts.
