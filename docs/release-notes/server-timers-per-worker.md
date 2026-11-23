---
type: improvement
---

- `lore-server`: a server whose requests set many timers at once, such as a burst of reads that
  miss the local store and go to its peers, keeps its workers busier with requests and less with
  waiting on each other. Clients and programs that embed Lore are unchanged. Set
  `LORE_CORE_TIMER=shared` to keep the earlier timer (see the Lore Server configuration
  reference).
