---
type: fix
---

- `lore-revision`: Fixed a link removed and added back at the same pin with other branching, such as
  `--disable-branching`, failing to commit with `Nothing staged for commit`. `lore unstage` and `lore reset`
  of the link put its committed branch and flags back, so `lore branch switch` is no longer refused,
  where `lore unstage` left the change staged and `lore reset` failed with `Failed to reset staged node`
- `lore-revision`: Fixed `lore unstage` and `lore reset` of a link replaced by a link to another repository
  writing the old link's files before failing with `Link not found`. They refuse before changing anything
- `lore-server`: The thin client revision diff announces the repository of a link whose only change is
  its flags, as it does for a link whose branch or pin changed
