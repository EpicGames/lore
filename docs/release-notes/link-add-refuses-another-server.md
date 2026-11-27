---
type: breaking
---

- `lore-revision`: `lore link add` and `lore_link_add` refuse a repository URL whose host or port
  differs from the repository's remote with an invalid arguments error (exit code 3,
  `LORE_ERROR_CODE_INVALID_ARGUMENTS`), before contacting any server, since every later fetch of
  the link goes through that remote. Hosts are compared without case but not resolved, so a URL
  naming the remote's server as `localhost` where the remote says `127.0.0.1`, which linked before,
  is refused: name the repository by its name or ID instead. A URL is refused with `NoRemote` when
  the repository has no remote, and a URL on the remote's server is resolved through the remote
  whatever its scheme.
