---
type: fix
---

- `lore-revision`: Fixed `lore branch merge abort`, with or without `--link`, writing files
  conflicted inside a link, or the link's mount directory, where the view excludes them. Commands
  that realize a link now ask the view about its mount as a directory, so a directory rule
  excludes the mount unless a rule re-includes something below it.
