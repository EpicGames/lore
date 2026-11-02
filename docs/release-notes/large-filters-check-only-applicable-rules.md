---
type: improvement
---

- `lore-revision`: `lore status --scan`, `lore stage --scan` and `lore sync --reset` are faster under a view or
  ignore file of about a thousand rules or more: each path is checked only against the rules that can apply to it,
  found by its leading directories. A view or ignore file holding thousands of paths in one directory also loads
  faster. Under a view of 6,342 rules over 450,000 files, a scan takes a seventh of the CPU time.
