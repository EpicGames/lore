---
type: fix
---

- `lore-storage`: Fixed `lore branch list` asking the remote about each branch name archived in the
  local store, and `lore_storage_mutable_list` listing keys that hold zero. The local mutable store
  kept an entry holding zero for a key stored or compare-and-swapped to zero, and created one for a
  compare-and-swap from zero to zero, where zero removes the key. It now removes the entry, and a
  list leaves out a key holding zero, an entry an older version wrote included.
- `lore-server`: Fixed Lore Server's DynamoDB mutable store keeping an item holding zero for a key
  compare-and-swapped to zero, and creating one for a compare-and-swap from zero to zero, as a
  branch created without revisions makes. It now deletes the item on the swap's condition, and
  writes none for a swap from zero to zero.
- `lore-server`: Fixed a DynamoDB compare-and-swap reporting success when DynamoDB refused it while
  returning an item that holds the expected value. It now fails with an internal error.
