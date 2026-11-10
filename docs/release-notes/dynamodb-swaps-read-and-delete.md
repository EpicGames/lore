---
type: improvement
---

- `lore-server`: Lore Server's DynamoDB mutable store answers a compare-and-swap whose new value is
  the value it expects with a strongly consistent read, which writes nothing, instead of a
  conditional write. The operation-latency metric times such a swap as `get_item`, and a swap to
  zero as `delete_item`, where it timed both as `put_item`, and its `delete_item` series gains a
  `conditional` label.
