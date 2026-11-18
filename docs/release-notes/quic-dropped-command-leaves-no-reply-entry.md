---
type: fix
---

- `lore-transport`: Fixed a QUIC command that was dropped before it was sent, or rejected as too
  big, holding memory until its connection's stream closed.
