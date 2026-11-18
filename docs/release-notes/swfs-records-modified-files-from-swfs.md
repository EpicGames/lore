---
type: improvement
---

- `lore-revision`: An SWFS instance records the files SWFS reports as changed, and `lore status --scan`, staging and the
  dirty checks look only at those, rather than walking the whole tree through the mount. After the service mounts an
  instance, or the instance moves to another revision, the first operation learns what differs from SWFS's own list of
  changed files, and walks the tree only for a renamed file whose old name SWFS does not list
- `lore-revision`: An SWFS instance serves only what its view and a clone's `--root-file` dependency filter include
  ([ADR-00034](../developing/decisions/00034-swfs-serve-views-and-partial-trees.md)), where it listed and read the whole
  revision. Changing the view, or a sync that brings new files, keeps a local change the view or the sync does not touch
