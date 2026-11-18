---
type: fix
---

- `lore-revision`: Fixed `lore status --scan` on an SWFS instance reporting a new directory only on the first scan,
  keeping the dirty flag of a file changed back to what the revision holds, and keeping an unstaged add whose file was
  removed
- `lore-revision`: Fixed `lore status`, staging and the dirty checks failing on an SWFS instance with `SWFS froze no
  file for <path>, which changed before the last thaw` for a file changed and since restored, such as by `lore reset`
  or a branch switch
- `lore-revision`: Fixed `lore stage` without `--scan`, run after a scan on an SWFS instance, staging new files as
  deletes and leaving edited and removed files unstaged
- `lore-revision`: Fixed `lore status` on an SWFS instance after a restore failing with `SWFS lists a frozen file ...
  but gives no content for it` for a file changed only in name or attributes
