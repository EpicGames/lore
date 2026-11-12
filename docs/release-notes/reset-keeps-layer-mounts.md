---
type: fix
---

- `lore-revision`: Fixed `lore reset` without `--purge` deleting a layer's files, local edits included, when it deleted a directory holding the layer's mount, such as `lore reset --last-merged-from <branch> <directory>` did for a directory the revision it resets to lacks. The reset deletes the rest of the directory and leaves the mount, as it does with `--purge`. A reset that would put a file in place of a directory holding a layer's mount fails with `Operation not supported`, and changes nothing, with or without `--last-merged-from`, whichever of the paths it names is refused.
