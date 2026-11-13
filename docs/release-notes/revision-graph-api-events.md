---
type: breaking
---

- `lore-capi`: `enum lore_event_id_t` gains `LORE_EVENT_REVISION_GRAPH_BRANCH`,
  `LORE_EVENT_REVISION_GRAPH_INSTANCE`, `LORE_EVENT_REVISION_GRAPH_REVISION` and
  `LORE_EVENT_REVISION_GRAPH_LOCATION_COMPLETE` after `LORE_EVENT_REPOSITORY_INSTANCE_CLONE_COMPLETE`,
  and `lore_event_t` gains a member for the data of each. Existing tags keep their values,
  `lore_event_t` keeps its size, and only the revision graph calls send the new tags, so a program
  built against an earlier `lore.h` keeps working without a rebuild. Rebuilt against the new
  `lore.h`, a `switch` over `lore_event_id_t` with no `default` case warns about the new tags under
  `-Wswitch` (part of `-Wall`): handle them or add a `default` case.
