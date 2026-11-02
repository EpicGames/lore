---
type: improvement
---

- `glob-match`: a view or ignore rule whose fixed start or end differs from a path's, such as `/Game/**/*.uasset`
  against `Engine/Source/x.cpp`, fails without a full match. A query against a 100-rule ignore file takes a fifth of
  the time, and `lore status --scan` under a view of 6,342 rules over 450,000 files takes about 11 s instead of
  157 s.
