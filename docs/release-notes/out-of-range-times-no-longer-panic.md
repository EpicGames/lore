---
type: fix
---

- `lore-client`: Fixed the CLI panicking when it prints a time outside the years 0 to 9999, such
  as a token's expiry or the time a lock was taken. It leaves the time out.
- `lore`: Fixed Lore panicking where it reads the current time while the system clock is set
  before 1970.
