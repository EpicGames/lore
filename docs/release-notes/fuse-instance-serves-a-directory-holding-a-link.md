---
type: fix
---

- `lore-revision`: Fixed a FUSE virtual instance failing with an I/O error to list a directory
  holding a repository link, a link at the root included, and status and `lore link update` failing
  with it.
- `lore-revision`: Fixed `lore stage --scan` on a FUSE virtual instance, after a sync brought a link,
  staging the linked repository's files as deleted, so that the commit deleted them there. Nothing at
  or below a link is reported or staged.
- `lore-revision`: Fixed a merge bringing a link on a FUSE virtual instance, and `lore reset` of a
  path at or above a link there, writing the linked repository's files into the instance as files
  of its own. The merge records the link, and its commit takes it. `lore link update`,
  `lore link remove` and a reset undoing either work there too; `lore link add` and a
  commit of a link alone stay refused.
- `lore-revision`: Fixed a sync or branch switch onto a revision adding a link serving it on a FUSE
  virtual instance as a plain directory, and a restart of the service turning a link into a local
  directory where the instance held local changes at or below it. It stays a link.
