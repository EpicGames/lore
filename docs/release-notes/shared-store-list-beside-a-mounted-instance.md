---
type: breaking
---

- `lore`: Fixed `lore shared-store list --include-instances true`, and `lore_shared_store_list` with
  `include_instances` set, waiting until the service stopped when run outside the service while it
  served a virtual instance.
- `lore`: A listing of the shared stores with their instances waits 3 seconds in all for the stores
  other processes hold, and lists a store another process still holds after that without its
  instances, with a warning naming it, and succeeds. It used to wait for each store and list it in
  full.
- `lore`: With `include_instances` set and a service running in another process, the listing runs
  in the service, so a store the calling process holds is listed without its instances, with the
  same warning.
- `lore`: Before acting on the instances either listing gives for a store, such as deleting the
  store, check that no warning names it, and list again once the process holding it is done, which
  for the calling process means once it has closed its storage handle on the store and let its
  `store_keep_alive` window end.
