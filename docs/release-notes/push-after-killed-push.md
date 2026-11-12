---
type: fix
---

- `lore-revision`: Fixed `lore status` reporting the branch behind or diverged from the remote
  after a commit or sync killed before it stored the branch latest, then a push of the same
  revision killed once the remote had accepted it. The push run after it records the revision as
  the branch latest, so the branch reads as in sync with the remote.
