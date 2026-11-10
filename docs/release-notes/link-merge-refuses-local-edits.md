---
type: fix
---

- `lore-revision`: Fixed `lore branch merge start` checking paths inside a link at the repository root, not the mount.
  A local edit or untracked file in the way there is refused with `LocalModifications` before the merge starts.
- `lore-revision`: `lore branch merge start` checks every link, the main repository and carried link pins first.
  Local work in any of them refuses the whole merge before anything is merged, leaving no merge to abort.
