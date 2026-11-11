---
type: fix
---

- `lore-revision`: Fixed `lore branch merge`, `lore push --fast-forward-merge`, `lore revision cherry-pick`,
  `lore revision revert` and `lore sync` losing a change that only gives a file the executable bit or takes it away.
  The merge keeps it beside the other side's edit of the file or of other files, its move, a rename that only changes
  case, and the text merge of edits on both sides, and never reports a conflict for the bit. Where the other side
  deletes the file, puts another file in its place or replaces a directory above it with a link, that change wins. A file
  added at a path after a revision deleted the file there is a new file, and takes its own mode.
- `lore-revision`: Fixed the same commands reporting a conflict where one side moves a file and changes its executable
  bit and the other edits the file.
- `lore-revision`: Fixed the same commands losing a change that only mounts another directory of the revision a link
  pins, where the other side leaves the link as it was. Where both sides change the link and keep its pin, the diff
  takes neither change: `lore branch merge` and `lore sync` settle the two mounts in their link merge, and the other
  commands keep the link the side merged into holds.
- `lore-revision`: Fixed, on Linux and macOS, `lore branch merge resolve mine` and `theirs`, and the same resolves of
  `lore revision cherry-pick` and `lore revision revert`, giving a file the executable bit of the side resolved to.
  A resolve chooses the content where the two sides' content conflicts, and never undoes a change of the bit: every
  file it touches, conflicted or merged cleanly, keeps the bit the merge gives it.
- `lore-revision`: Fixed, on Windows, the same resolves committing the merge's executable bit for a file the side
  resolved to deleted and added again. The new file keeps its own bit.
- `lore-revision`: Fixed `lore sync`, `lore branch switch`, `lore branch merge`, `lore revision cherry-pick`,
  `lore revision revert` and `lore link update` giving a file the executable bit the incoming revision gives it, or
  taking it away, and then refusing for a local change to another file, a layer's sync included. They change no bit
  before every file has passed.
