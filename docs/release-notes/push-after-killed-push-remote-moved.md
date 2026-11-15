---
type: fix
---

- `lore-revision`: Fixed `lore push` failing with `Branch has diverged` when run again after a push
  killed once the remote had accepted its revision, where another instance had pushed on top in
  between: the push records the revision and leaves the branch behind the remote.
- `lore-client`: Fixed `lore push` printing `Revision is already pushed and remote has moved forward`
  only on `main`, and also when the push then failed.
