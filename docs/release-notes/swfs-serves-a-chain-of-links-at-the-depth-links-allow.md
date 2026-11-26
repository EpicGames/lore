---
type: fix
---

- `lore-revision`: Fixed an SWFS virtual instance serving nothing at a link whose source is itself
  a link, where the chain runs exactly as deep as links may nest. It is served, as a shorter chain
  is.
