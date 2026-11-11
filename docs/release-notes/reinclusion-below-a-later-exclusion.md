---
type: fix
---

- `lore-revision`: Fixed a view or an ignore file bringing back a path that a rule re-includes below a directory a
  rule after it excludes, where an earlier rule excluded the directory too. With `/**`, `!/a*`, `!*/d/y` and `/a/*`,
  `a/d/y` came back; it stays out, as it does without `/**`. An exclusion in `.lore/ignore` keeps such a path out
  too, as `/Game/Saved/` does for `Game/Saved/Config/game.ini` under `Saved/` and `!/Game/Saved/Config/`.
