---
type: breaking
---

- `lore`: Fixed the mounts of one linked repository drifting apart. A commit through one mount,
  `lore link update` and unstaging an update move every mount of the repository, and
  `lore link add` of a repository already mounted pins the revision, branch and flags of its other
  mounts. A script that reads the text of `lore link list` has to read its mounts from the
  `Mounts:` lines under each linked repository, or use `--json`, which is unchanged. `lore link
  add` refuses a `--pin` other than the revision the other mounts pin, and `--disable-branching`
  where they follow branches. A change that would leave the mounts on different branches or flags,
  or a commit that would leave them on different revisions, fails with the new error
  `LinkPinsDiverge`, exit code 53.
