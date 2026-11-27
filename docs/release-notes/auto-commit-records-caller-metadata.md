---
type: feature
---

- `lore`: `lore_revision_sync` and `lore_branch_merge_start` take `metadata_keys`,
  `metadata_values` and `metadata_formats`, the metadata `lore_revision_commit_with_metadata`
  takes, and record it on the merge revision a diverged sync or a conflict-free merge commits. A
  caller that connects as an account id can name the author in `created-by` as it does for a plain
  commit. Empty metadata leaves that revision as before. Unlike `--inherit-metadata`, which carries
  keys over from the source revision, these are the caller's own values.
