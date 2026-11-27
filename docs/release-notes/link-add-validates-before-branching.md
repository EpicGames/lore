---
type: fix
---

- `lore-revision`: Fixed `lore link add` creating a branch in the linked repository when the add
  is then refused, and accepting a linked repository or branch with no revision, which only failed
  at commit with `Link head is zero`. Every check now runs before the branch is created or
  anything is written, and a repository with no revision is refused up front.
- `lore-revision`: Fixed `lore link add` refusals that did not name what they refused. A missing
  source path names the path, the linked repository, and the revision and branch it was looked up
  in. An overlapping mount names the mount path being added, and both mounts as the working tree
  shows them.
- `lore-revision`: Fixed `lore link add` and `lore branch merge` reading a failed query of the
  remote branch as a missing branch: link add then created the branch in the linked repository,
  and merge merged the local latest only. Both now report the failure.
