---
type: feature
---

- `lore-capi`: `lore_storage_get_file` writes the holes of content stored with `skip_holes` back
  as holes on macOS. APFS allocates the small holes beside data as zeros when it writes
  that data out, so a get on macOS deallocates the whole blocks of each hole once it has written
  every range of data, before it completes. It never deallocates written data, so written zeros
  stay written and allocated. A resumed get on APFS resumes from a temporary file whose holes APFS
  filled, rather than writing it again, since the same step deallocates them. A put with
  `skip_holes` on macOS syncs the file before it looks for holes, so a hole APFS fills this way is
  stored as data whether or not the file was written out before the put, and the file gets one
  address.
