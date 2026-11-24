---
type: fix
---

- `lore-revision`: Fixed `lore sync`, `lore branch switch`, `lore branch merge`, `lore revision cherry-pick` and `lore revision revert` under an unchanged view leaving a directory that an instance freshly cloned at the revision with that view doesn't hold, or skipping one it does hold: a directory left holding only what the view excludes is removed once empty, one the revision empties is created, and `lore status --scan` reports nothing afterwards. A directory holding an untracked or ignored file stays.
- `lore-revision`: Fixed `lore sync --view` leaving behind an empty directory the new view excludes when an incoming move takes out the last of its content.
