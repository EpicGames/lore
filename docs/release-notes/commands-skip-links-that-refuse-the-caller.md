---
type: fix
---

- `lore-revision`: Fixed commands failing with "Not authorized" for a caller without access to a
  linked repository. `lore link list` lists the other links; `lore status --scan`, `lore stage`,
  `lore reset`, `lore sync`, `lore branch switch` and `lore revision revert` work around the link,
  taking a pin another user moved without reading the linked repository; and `lore branch create`
  and `lore branch archive` with links leave that repository alone, the new branch keeping its
  link at its pin and branch. `lore link add`, `link update` and `link remove` still fail, naming
  the link's path and repository. A sync or switch that removes the mount of such a link, or a
  directory around it, refuses as for local modifications only while the mount holds files.
