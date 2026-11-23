---
type: improvement
---

- `lore`: Log files rotate at midnight UTC in place of local midnight, and a rotated file is named
  after its UTC date, the date its lines are stamped with. The CLI always writes a log file, so
  this reaches every CLI user outside UTC. Read the date in a rotated file's name as UTC.
