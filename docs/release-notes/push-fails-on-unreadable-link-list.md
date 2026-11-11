---
type: breaking
---

- `lore-revision`: a push of a revision whose links can't be read fails with the read's error, such as
  `AddressNotFound`, rather than pushing the revision without its links.
