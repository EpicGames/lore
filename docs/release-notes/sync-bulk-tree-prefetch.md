---
type: feature
---

- `lore-revision`: `[sync] prefetch_tree = true` in an instance's `config.toml` makes a sync fetch the target's tree
  blocks and name tables in bulk requests instead of one per block as the walk reaches it. It fetches the whole tree
  whatever the view, so it is off by default
