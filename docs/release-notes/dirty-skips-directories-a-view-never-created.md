---
type: fix
---

- `lore-revision`: Fixed `lore dirty .` in an instance with a view marking deleted a directory the instance never held, such as `Engine/` under `/*` and `!*/Saved/Config/` where the repository holds no `Saved/Config` below it, which `lore status` then listed under changes not staged for commit.
