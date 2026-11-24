---
type: fix
---

- `lore-revision`: Fixed `lore repository instance list` and `lore repository instance prune` failing with
  `Address not found` where one instance registration names data the local store doesn't hold. The list leaves out such
  a registration, and one whose data the local store fails to read, with a warning, and lists the other instances.
  `lore repository instance prune`, and the prune that `lore clone` runs over a shared store, remove a registration
  whose data is missing from the local store or malformed, and, where they find any, then register the current instance
  again where it has no registration. The same registration also failed `lore shared-store list --include-instances`, cloning or creating a
  repository over the shared store, moving or cloning an instance, and `lore service set-external-directory`. It also
  dropped the warning that another instance is on the branch at a branch switch or sync, and kept the service from
  mounting the virtual instances of the store.
