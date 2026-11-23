---
type: fix
---

- `lore-client`: Fixed `lore sync` and `lore status` reporting a missing, expired or rejected login as
  "not authorized or unavailable", or, in `lore status`, saying nothing about the remote. Both now say the caller is not
  logged in and suggest `lore auth login`. A sync still falls back to local history and exits 0
- `lore-client`: "Remote reachable but could not read remote revision (not authorized or unavailable)" reads "Could not
  read the remote revision (not authorized or unavailable)". `lore sync` prints it when the remote revision can't be read
  for another reason, including when the remote can't be reached. `lore status` prints it when the remote is reached but
  the branch query fails
