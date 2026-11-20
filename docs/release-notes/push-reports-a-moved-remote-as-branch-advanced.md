---
type: breaking
---

- `lore-revision`: `lore branch push` and `lore_branch_push` report a push the remote refuses
  because its latest revision has moved on as `BranchAdvanced` (41), where they reported
  `Internal` (-1, exit status 255): a push that isn't a fast-forward, a fast-forward merge that
  conflicts, and a push onto a branch that has diverged from the remote's. The message still says
  why. A script that tests these refusals for exit status 255 or code `-1` tests for 41 as well.
- `lore-base`: The message of `BranchAdvanced` (41) reads "The branch's latest revision has moved
  on; sync to bring in its new revisions", where it read "Branch has been advanced by another
  instance, sync and re-stage to commit". `lore branch push` and `lore commit` print it. The code
  and the exit status don't change.
- `lore-revision`: A `lore branch push` or `lore_branch_push` that pushes a linked repository's
  branch the remote has moved past reports `LinkDivergent` (52) only for the remote's refusal of
  that push; another failure of it, such as a failed upload, keeps its own code, where it
  reported `LinkDivergent`. Against an older Lore Server, which doesn't mark its refusal, that
  refusal fails as `Internal` (-1) with the server's message, not `LinkDivergent`.
- `lore-server`: Lore Server marks its refusal of a push that isn't a fast-forward, and of a
  fast-forward merge that conflicts, in the refusal's status details, and the client reports only
  a marked refusal as `BranchAdvanced`. A pre-push hook's `FAILED_PRECONDITION` stays `Internal`.
  Against an older Lore Server, which doesn't mark its refusal, a push that isn't a fast-forward
  or whose fast-forward merge conflicts still fails as `Internal`. An older client ignores the
  mark.
