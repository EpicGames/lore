---
type: fix
---

- `lore-revision`: Fixed `lore link update` refusing every link with `Link has filesystem changes` while the parent
  had anything staged. It refuses where `lore sync` refuses, where realizing the new pin would overwrite or delete a
  local change, whatever the parent has staged.
