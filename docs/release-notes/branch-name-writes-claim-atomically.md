---
type: fix
---

- `lore-revision`: Fixed concurrent creates, with `lore branch create` or the C API, of one name, or
  of one new branch id under two names, all succeeding, which left a branch without its name. One
  now succeeds, and the others fail with `BranchAlreadyExists` (57). A create that loses the name
  removes the metadata it wrote.
- `lore-revision`: Fixed restores and archives of one branch that race undoing each other's writes.
  Concurrent creates that restore an archived branch by its id all succeeded, and left it mapped
  from each of their names. A restore under a new name could rename a branch whose create hadn't
  finished. `lore branch archive` could release the name of a branch a concurrent restore had
  renamed, which left it live, and a failed restore could remove the name another restore took. Of
  racing restores at most one now succeeds, and the others fail with `BranchAlreadyExists` (57), as
  does a restore that would rename an unfinished branch. A branch without a latest revision that an
  earlier version archived is still restored under its old name. An archive archives the branch as a
  concurrent restore leaves it. The next create or archive of the branch, or
  `lore branch create --force` of the name, settles a restore that stopped part way, as when its
  process ended, and frees the name it claimed. Branch metadata gains `archived` and `restored`,
  times in milliseconds since the Unix epoch, `restoring`, the name a restore in progress claims,
  and `released`, the name a settled restore claimed. An archive and a restore each write a metadata
  blob to the immutable store, as a create does.
- `lore-revision`: Fixed writes that undid a concurrent writer's change. A restore wrote its
  metadata over a protect or a metadata edit made while it ran. `lore branch protect` and
  `lore branch unprotect` without a remote put back the old name of a branch a concurrent restore
  renamed, after which an archive of it failed with `BranchNotFound`. A clone overwrote the name of
  a branch it records that another writer had claimed. `lore branch archive`, and
  `lore branch create --force` replacing a branch, removed a name mapping a concurrent writer had
  pointed at another branch. Each now keeps the other writer's change: the clone logs a warning, and
  the forced create fails with `BranchAlreadyExists` (57).
- `lore-revision`: Fixed a create or a clone taking a read that failed as one that found nothing. A
  create wrote fresh metadata over the branch whose id it was given, or took a name from the branch
  it maps to, and a create given an archived branch's id took the branch's old name as free. A clone
  recorded a name whose mapping it couldn't read. They now fail with the read's error, a remote
  refusal included, so `lore branch create` fails while the remote refuses the caller. A create
  whose remote can't be reached still succeeds. `lore branch archive` of a branch only the remote
  holds now fails with the remote's refusal, where it failed with `BranchNotFound`.
- `lore-revision`: Fixed a create given an archived branch's id being refused when the branch's old
  name now names another branch. It restores the branch under the new name. Fixed
  `lore branch create --force` of a name mapped to a branch that records neither the name nor a
  restore under it archiving that branch and then failing. It now takes the name and leaves the
  branch as it was.
- `lore-server`: Fixed the branch delete, the branch list's repair of a missing default-branch
  mapping, and the repository delete changing name mappings other writers had made. A delete removed
  a mapping a concurrent writer had pointed at another branch, the repair overwrote a name another
  writer claimed first, and a repository delete left behind each name that maps to a branch whose
  metadata names it otherwise. Of two concurrent deletes of one branch only one is now counted,
  notified and runs the post hook, and a repository delete releases every name mapping.
- `lore-server`: Fixed a branch create dropped part way, at its deadline or by its client, leaving
  an archived branch half restored. A create and a delete now run to their end, and one dropped is
  still counted, notified and runs the post hook. Fixed the branch create answering a store failure,
  such as a failed read of the name's mapping or of a branch's metadata, as `INVALID_ARGUMENT`, and
  logging nothing. It now answers `INTERNAL` and logs a warning.
- `lore-server`: Fixed the branch create making a branch with id zero for a request whose id is zero
  or unset. It now answers `INVALID_ARGUMENT`.
- `lore-server`: Fixed the deprecated `urc.rpc` `BranchProtect` and `BranchUnprotect` requests
  putting back branch metadata that a restore changed while they ran.
