---
type: fix
---

- `lore-revision`: Fixed `lore branch merge start` merging a repository twice where two links
  reach it, such as two linked repositories that both link it, which failed the commit. The merge
  now merges it once and pins both links at the result, and a commit commits it once. It refuses
  the merge before writing anything where the links pin the repository at different revisions or
  on different branches, or where its merge can conflict. A commit refuses different per-link
  messages for links pinning one staged revision, which it commits once.
- `lore-revision`: Fixed `lore commit` of edits staged under two links to one repository failing
  after it had moved the linked repository's branch, which the next push then published. The
  commit refuses such edits, two staged revisions of one repository on one branch, and
  `lore commit --link` of one of the links alone, before it writes anything.
