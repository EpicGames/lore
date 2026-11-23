---
type: fix
---

- `lore-revision`: Fixed `lore link update` replacing the files of a revision committed inside the
  link and not pushed with the linked branch's remote latest. Without `--pin`, a link pinned ahead
  of the remote branch now keeps its pin and says to push, and a link whose pin diverged from the
  remote branch refuses with a divergent error, leaving the mount and the pin as they were and
  naming `lore --force link update` and `--pin` to drop the unpushed revisions.
