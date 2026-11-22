---
type: fix
---

- `lore-revision`: Fixed `lore layer add` and `lore link add` mounting a layer and a link at, inside
  or above each other: the layer's files were cloned into the link's mount, or the link was staged
  and cloned over the layer's files. Both commands now refuse such a mount, naming it and the mount
  it overlaps and how to resolve it, before they create a branch in the layer or linked repository.
