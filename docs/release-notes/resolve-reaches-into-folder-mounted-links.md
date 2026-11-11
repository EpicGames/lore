---
type: fix
---

- `lore-revision`: Fixed resolving a folder that holds a link, such as the whole tree, failing with `NotSupported` at a
  link that holds conflicts. It now resolves the conflicts inside each link the merge merged
- `lore-revision`: Fixed `lore branch merge abort` failing where a linked repository holds a link of its own
- `lore-revision`: Fixed `lore branch merge start`, run again to merge the parent once the conflicts inside its links
  are resolved, failing with "File has local changes" where a conflict inside a link was marked resolved with the
  content on disk, and dropping that resolution from the merge
- `lore-revision`: Fixed `lore branch merge start`, run again while a conflict inside a link is unresolved, merging the
  parent anyway. It now stops and lists the conflicts left inside the links
- `lore-revision`: Fixed `lore branch merge resolve` inside a link mounted from a folder resolving a file outside
  the folder, or writing the chosen side to a path below the mount that the link does not hold
- `lore-revision`: Fixed `lore branch merge abort` restoring a conflicted file inside a link mounted from a folder
  with the content of the file at the same path outside the folder
