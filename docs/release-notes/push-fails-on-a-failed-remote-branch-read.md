---
type: fix
---

- `lore-revision`: Fixed `lore branch push` and `lore_branch_push` taking a branch whose remote
  record they failed to read as absent from the remote: the branch pushed, a branch it was created
  from, or the branch of a layer or linked repository it pushes. The push tried to create such a
  branch, or walked its history as history the remote lacks. It now fails with that read's error, such as
  `Disconnected` (28) or `NotAuthenticated` (16), before it uploads anything. A branch the remote
  doesn't hold is still created on its first push.
