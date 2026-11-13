---
type: fix
---

- `lore-revision`: Fixed a name lookup, as `lore branch switch` or `lore branch create`
  (`lore_branch_create`) makes it, mapping the name of an archived branch, whose id an earlier
  version derived from its name, back to it. The branch read as live, and a create of a new branch
  under the name failed with `BranchAlreadyExists` (57). The branch now stays archived and its name
  stays free, and so does the old name of such a branch restored under another name.
  `lore branch create` given the id of such an archived branch now restores it, where it failed with
  `BranchAlreadyExists`. One that v0.13.0 or an earlier release archived still reads as live.
- `lore-revision`: Fixed `lore branch create` (`lore_branch_create`), and Lore Server's branch
  create, taking a name with no mapping where the store couldn't read the branch whose id derives
  from that name, which may be live. A failed read of that branch's latest revision or metadata now
  fails the create with the read's error, as it does a restore under another name,
  `lore branch archive` of a branch that had the name, and `lore revision history --branch`, which
  failed with `BranchNotFound`. It also fails the commands that resolve a branch by name, such as
  `lore branch switch`, `lore branch push`, `lore branch info`, `lore branch diff` and the lock
  commands, which asked the remote instead. `lore branch switch` over layers logs a warning and
  skips a layer whose branch create fails this way, where it took the name.
