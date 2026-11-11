---
type: fix
---

- `lore-revision`: Fixed content a forced stage takes past the view. `lore commit` failed with
  "Node with zero hash remain after nodes were committed" after `lore --force stage --scan`
  staged a new file the view excludes, and committed the revision's content in place of an edit
  there. The commit reads such a file from the working tree, and `lore status` lists it as
  staged. `lore unstage` follows the view and leaves it staged; `lore --force unstage` takes it
  back. Below a layer mount, `lore unstage` doesn't follow the view yet.
- `lore-revision`: Fixed `lore --force stage` of a file a merge in progress staged past the
  view. The merge commit recorded the merged content with the size of the working tree's file.
  It takes the working tree's file, its content and size, as the merge's resolution, and records
  it as merged, as it does for an edit of a merged file in view.
- `lore-revision`: Fixed `lore status` leaving out what a merge in progress staged at paths the
  view excludes. A path where the merge took the incoming side shows as a staged change with that
  side taken.
- `lore-revision`: Fixed `lore status` listing a file a merge took without a conflict as an
  unresolved conflict after `lore branch merge resolve theirs` or `resolve mine` on a directory
  holding it. It is listed as a staged change with the side taken, and the status event of the C
  API carries `flag_conflict_unresolved` unset for it.
- `lore-revision`: Fixed `lore unstage` of a directory added in the stage taking back what the
  view excludes below it. Where the view could take in something below the directory, as
  `!*/Saved/Config/` can below every top-level directory, the unstage took back every file below
  it. It takes back only what the view keeps, and the directory stays staged while anything below
  it does.
- `lore-revision`: Fixed `lore --force stage`, or a stage of an unresolved conflict, of a file
  staged as a move, or staged as an add by a merge. The commit recorded it as a modification. It
  keeps the move or the add, with the working tree's content.
- `lore-revision`: Fixed `lore unstage` of a directory or a link holding a change past the view,
  which the unstage leaves staged. The commit recorded the directory as the current revision
  holds it, without the change. The commit left out the change below a link, and status no
  longer listed it. `lore unstage .` took the change back where nothing else stayed staged or
  dirty. The directory or link stays staged while anything below it does.
