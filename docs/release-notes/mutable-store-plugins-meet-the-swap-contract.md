---
type: breaking
---

- `lore-server`: Make a mutable store plugin's compare-and-swap to zero remove the key, its swap
  from zero to zero write nothing, and its list leave out a key holding zero, as the local and
  DynamoDB stores do. A plugin that keeps a key holding zero keeps one for each branch name a
  branch or repository delete releases, and Lore Server's branch list warns about each of them on
  every listing.
- `lore-server`: Make a mutable store plugin's compare-and-swap refuse a swap from a non-zero
  value on an absent key, answering zero, and answer a value the key held while the call ran. A
  plugin that makes such a swap, or answers a value read before the call, reports a name released
  or branch metadata swapped that the key didn't hold.
