---
type: fix
---

- `lore-revision`: Fixed sync, commit and clone progress events that could report discovery as
  incomplete after an earlier event reported it complete, report a count that only grows lower than
  an earlier event did, or arrive after the operation's last progress event or its end event.
