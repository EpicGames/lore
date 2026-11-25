---
type: improvement
---

- `lore-server` (AWS store): an S3 or DynamoDB request whose store operation stops waiting for it, such as the
  `GetObject` and `HeadObject` the immutable store starts beside a fragment lookup that then finds nothing, is
  cancelled rather than left to run to its end. A request still waiting for a connection under `max_connections` is
  never sent. A write cancelled after it was sent may or may not have been applied, as after a dropped connection
