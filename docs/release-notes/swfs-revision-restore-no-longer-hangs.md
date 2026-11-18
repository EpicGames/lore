---
type: fix
---

- `lore-revision`: Fixed `lore revision restore`, and any commit or other operation run on an SWFS instance after a sync
  and before a scan, hanging the service. The operation read every file through the mount while its record of modified
  files was out of date, which left no I/O threads for SWFS's requests back to Lore
