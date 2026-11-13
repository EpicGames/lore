---
type: feature
---

- `lore-server`: `[topology.fixed]` takes an optional `self_address`, and leaves out the peer whose `address`
  is exactly equal to it. Servers sharing one `peers` list set it to their own address, for example through
  `LORE__TOPOLOGY__FIXED__SELF_ADDRESS`, so they don't replicate to themselves.
