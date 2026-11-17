---
type: improvement
---

- `lore-server`: a `lore.revision.v1` `BranchList` call that the store throttles answers
  `RESOURCE_EXHAUSTED` with a message that says what to do: do not list branches in a loop, and use
  `lore branch info --remote <branch>` for one branch. Other calls keep the message
  `Store overloaded, slow down`.
- `lore-transport`: a command that lists the remote's branches, such as `lore branch list`, prints
  the server's reason as a warning on the first throttled answer, before it retries. An error for
  a throttled call carries the server's reason on its trace.
