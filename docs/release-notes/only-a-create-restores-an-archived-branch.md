---
type: breaking
---

- `lore-revision`: `lore branch switch` and `lore branch reset` (`lore_branch_switch`,
  `lore_branch_reset`) onto an archived branch, given its id, now leave the branch archived. They
  used to map its name to it again, and with `--force` took the name from another branch that
  held it. Restore the branch with `lore branch create <name> --id <id>`, before or after the
  switch or reset. A switch or reset given the id of a branch the remote holds live still maps its
  name in the local store, as a lookup by its name does.
- `lore-server`: A push to an archived branch, through `lore.revision.v1` `BranchPush` or the
  deprecated `urc.rpc` `BranchPush`, now fails with `NOT_FOUND` and leaves the branch archived,
  where it mapped the branch's name to it again. `lore push` answers it as it answers a
  branch archived during the push: it restores the branch with a create, given its id, and pushes
  again, so pushes from the `lore` client still restore the branch. Another client that pushes to
  an archived branch restores it with a branch create first. A push to an archived branch whose
  name maps to another branch with metadata still fails with `ALREADY_EXISTS`, as the create that
  restores the branch would, which now includes a holder whose metadata names it otherwise, in
  another case or not, and one whose holder's metadata key can't be read fails with `INTERNAL`,
  where Lore Server took the name from either.
