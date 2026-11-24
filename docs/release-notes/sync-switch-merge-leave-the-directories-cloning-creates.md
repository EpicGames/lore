---
type: fix
---

- `lore-revision`: Fixed `lore sync`, `lore branch switch`, `lore branch merge`, `lore revision cherry-pick`, `lore revision revert` and `lore sync --view` leaving behind an empty directory the view excludes when an incoming move takes out the last of what the view keeps in it. A directory holding an untracked or ignored file stays.
