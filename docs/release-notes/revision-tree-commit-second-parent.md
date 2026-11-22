---
type: breaking
---

- `lore`: `lore_revision_tree_commit` commits a merge revision: `options.parent_other` names the second parent, the
  merged revision, and zero commits a revision with one parent, as before. The loaded revision stays the first parent,
  so a caller loads the handle at the revision the branch continues and names the merged revision. The merge revision
  is numbered one above the higher of its parents, `lore file history` and later merging follow the merged line, and
  it commits even with no edits. A second parent that's the loaded revision, that the store doesn't hold, or that's
  given to a handle loaded from the zero revision, fails with `INVALID_ARGUMENTS` before anything is written.
  `lore_revision_tree_commit_options_t` and `lore_revision_tree_commit_args_t` grow by the new field: a caller compiled
  against an earlier `lore.h`, and each language binding, rebuilds against the new layout. A zeroed options value still
  commits one parent.
