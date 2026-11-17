---
type: fix
---

- `lore-revision`: Fixed changed store limits not applying to a store the process already has open,
  such as one a mount holds in the Lore service, until a restart. The next command that collects
  applies them. A store first opened by a command that doesn't collect, such as `lore clone` before
  it writes, now collects once a later command asks it to.
