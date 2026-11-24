---
type: fix
---

- `lore-revision`: Fixed a caller with write access to a repository and no access to a repository linked into it
  being unable to commit or push there. `lore commit` and `lore link list --staged` no longer fail on a link the
  caller can't read when nothing in it is staged. A push leaves out each link whose pin is one a revision the
  remote holds already has, such as the remote's latest revision of the branch, so it no longer plans or checks
  the linked repository for it. A push that changes the pin of a link the caller can't read is still refused
  before anything is pushed, and names the link and the linked repository.
