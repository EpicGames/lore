---
type: improvement
---

- `lore-revision`: `lore repository instance delete` renames the directory that holds an instance's files,
  the instance directory or an SWFS instance's external directory, aside to `<name>.deleting-<instance ID>` beside it
  before deleting the files, so a regular instance's path is free as soon as the rename lands, and deletes the files at
  background disk priority. A delete that fails part way leaves what remains under that name, and the error names it. A
  delete refuses to start when that name is already taken. An instance directory that is a mount point, which refuses
  the rename, is deleted in place. On Windows, an instance directory that is a volume mounted in a folder or a junction
  is not renamed: its files are deleted through it, and the mounted folder or junction is left in place. A volume
  mounted in a folder also keeps the hidden system entries at its root, such as `System Volume Information`.
