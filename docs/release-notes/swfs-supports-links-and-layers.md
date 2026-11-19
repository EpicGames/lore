---
type: feature
---

- `lore-revision`: An SWFS instance supports links and layers. The instance's one SWFS mount serves each from its own
  repository, a link at the pin the staged state holds and a layer at its current revision, so their files stay
  virtual rather than becoming local changes SWFS holds. `link add`, `link update`, `link remove`, `layer add`,
  `layer remove`, layer syncs, unstaging and resetting inside a link, merges and branch switches across links work as
  on a plain instance, and a scan whose changes lie outside the links reads nothing of their trees. A path that
  changes between file and directory, such as a link replacing a file, is one thaw. Limitation: a directory the user
  made, which SWFS holds as the user's own, stays after `reset --purge`, `link remove` and `layer remove --purge`,
  because no SWFS thaw removes one; the user's files in it are deleted
