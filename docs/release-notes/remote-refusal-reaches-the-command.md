---
type: breaking
---

- `lore-revision`: A command whose remote refuses the caller fails with `NotAuthenticated` (16) or
  `NotAuthorized` (17), where it reported a missing branch, failed as an internal error (exit status
  255) or went on without the remote. `lore branch merge into`, `lore revision restore` and
  `lore clone` of a repository with layers failed with 255. `lore branch merge` merged against the
  local latest revision of the branch, or failed with 255 when it had none. `lore branch diff`
  compared the branches' local latest revisions. A revision named by a branch's latest or by a
  number on it, such as `lore file reset --revision @LATEST`, resolved against the local latest
  revision of the branch. `lore link add`, `lore link update` and `lore history --branch` reported
  the branch as not found, and a three-way diff such as `lore file diff --diff3` failed with
  `InvalidArguments` (3), or on one branch with `Divergent` (42). `lore file obliterate` succeeded
  and left the remote copy in place, or, when the remote refused the obliterate request itself,
  failed after removing the local copy; it now fails before it removes anything. `lore branch create` failed with 255 when a linked repository
  or a layer refused the new branch, and `lore branch create --force` reported the branch as
  existing (57) when the remote refused the lookup of the branch to replace. Scripts that test
  these commands for 255 or a not-found code should test for 16 and 17 as well.
- `lore-revision`: A refusal while reading a revision's state is reported as the refusal, where it
  was an internal error. So is a refusal while `lore file diff --source` or `--target`,
  `lore file history`, `lore file write`, `lore file info` or `lore file reset` with `--revision`
  look up a revision on a branch the instance does not hold, while `lore file diff`,
  `lore file write`, `lore file obliterate` and `lore file info` look up the file, while
  `lore stage` looks up a path in the staged state, while `lore file reset` lists the staged links
  under its paths, while `lore file reset` and `lore file info --filtered` read the nodes below a
  directory, while `lore commit --link` looks up the link, and while `lore sync --root-file` looks
  up a root file. These reported the revision, the file or the link as not found, or failed with
  255, and `lore file info` reported a file in the working tree as added.
- `lore-revision`: `lore file reset --last-merged-from` fails with the refusal when the remote
  refuses a revision of the file's history that the reset walks back through, or the merged-in
  parent of a merge on the way, where it reset the file to an earlier merge or to the branch point.
- `lore-revision`: `lore file audit` reports a line as link-unavailable when the remote refuses to
  read the state of its earlier version in a linked repository, as it does for that version's
  content. Such an audit failed with 255.
- `lore-revision`: A fragment's metadata read through a composite store whose durable store refuses
  the caller fails with that refusal, where it answered that the fragment was not there.
