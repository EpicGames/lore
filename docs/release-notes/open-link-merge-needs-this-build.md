---
type: breaking
---

- `lore-revision`: While a merge that merges a linked repository or holds a link conflict is open,
  its staged state is written in a format earlier builds refuse with "Upgrade format", so a
  downgraded client cannot misread it. To go back to an earlier build, finish or abort the merge
  first. Committed revisions are unchanged.
- `lore-revision`: `lore link add` refuses a second mount of the same folder of a repository that
  follows another branch or carries other flags. Add it with the branch and flags the first mount
  has, or mount another folder.
